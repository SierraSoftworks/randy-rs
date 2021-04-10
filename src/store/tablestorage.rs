use crate::{models::*, trace_handler};
use crate::api::APIError;
use std::{fmt::Debug, pin::Pin, sync::Arc};
use futures::{StreamExt, Future};
use rand::seq::IteratorRandom;
use actix::prelude::*;
use azure_core::prelude::*;
use azure_storage::core::prelude::*;
use azure_storage::table::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument;

type TableReference = Arc<TableClient>;

pub struct TableStorage {
    started_at: chrono::DateTime<chrono::Utc>,

    ideas: TableReference,
    role_assignments: TableReference,
    collections: TableReference,
    users: TableReference,
}

const URI_CHARACTERS: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'%')
    .add(b'#')
    .add(b'&');

impl TableStorage {
    pub fn new() -> Self {
        let connection_string = std::env::var("TABLE_STORAGE_CONNECTION_STRING").expect("Set the TABLE_STORAGE_CONNECTION_STRING environment variable before starting the server.");

        let http_client: Arc<Box<dyn HttpClient>> = Arc::new(Box::new(reqwest::Client::new()));
        let client = StorageAccountClient::new_connection_string(http_client, &connection_string).expect("a valid connection string");
        let table_service = client.as_storage_client().as_table_service_client().expect("a valid table storage client");      
        
        let ideas_table = table_service.as_table_client("ideas");
        let role_assignments_table = table_service.as_table_client("roleassignments");
        let collections_table = table_service.as_table_client("collections");
        let users_table = table_service.as_table_client("users");

        Self {
            started_at: chrono::Utc::now(),

            ideas: ideas_table,
            collections: collections_table,
            role_assignments: role_assignments_table,
            users: users_table,
        }
    }

    #[instrument(err, skip(table, not_found_err), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "GET"))]
    async fn get_single<ST, T>(table: TableReference, type_name: &str, partition_key: u128, row_key: u128, not_found_err: APIError) -> Result<T, APIError>
    where
        ST: DeserializeOwned + Clone,
        T: From<ST> {
        let result = table
            .as_partition_key_client(&format!("{:0>32x}", partition_key))
            .as_entity_client(&format!("{:0>32x}", row_key))
            .map_err(|err| {
                error!("Failed to retrieve item from table storage: {}", err);
                APIError::new(500, "Internal Server Error", "We were unable to retrieve the item you requested, this failure has been reported.")
            })?
            .get().execute().await
            .map_err(|err| {
                error!("Failed to retrieve item from table storage: {}", err);
                APIError::new(503, "Service Unavailable", "We were unable to retrieve the item you requested, this failure has been reported.")
            })?;

        result.entity
            .ok_or(not_found_err)
            .map(|r: ST| r.into())
    }

    #[instrument(err, skip(table, filter), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "LIST", db.statement = %query))]
    async fn get_all_entities<ST, P>(table: TableReference, _type_name: &str, query: String, filter: P) -> Result<Vec<ST>, APIError>
    where
        ST: Serialize + DeserializeOwned + Clone,
        P: Fn(&ST) -> bool
    {
        let mut entries: Vec<ST> = vec![];
        let safe_query = TableStorage::escape_query(&query);

        if safe_query.is_empty() {
            let mut stream = Box::pin(table.query().stream::<ST>());
            
            while let Some(result) = stream.next().instrument(
                info_span!("Fetching page of results from Table Storage", "otel.kind" = "client", "db.system" = "azure_table_storage", "db.operation" = "LIST", db.statement = "*")
            ).await {
                let mut result = result
                .map_err(|err| {
                    error!("Failed to retrieve items from table storage: {}", err);
                    APIError::new(500, "Internal Server Error", "We were unable to retrieve the items you requested, this failure has been reported.")
                })?;

                entries.append(&mut result.entities);
            }
        } else {
            let mut stream = Box::pin(table.query().filter(safe_query).stream::<ST>());
            
            while let Some(result) = stream.next().instrument(
                info_span!("Fetching page of results from Table Storage", "otel.kind" = "client", "db.system" = "azure_table_storage", "db.operation" = "LIST", db.statement = %query)
            ).await {
                let mut result = result
                .map_err(|err| {
                    error!("Failed to retrieve items from table storage: {}", err);
                    APIError::new(500, "Internal Server Error", "We were unable to retrieve the items you requested, this failure has been reported.")
                })?;
                entries.append(&mut result.entities);
            }
        }

        Ok(entries.iter().filter(|&e| filter(e)).map(|e| e.clone()).collect())
    }

    #[instrument(err, skip(table, filter), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "LIST", db.statement = %query))]
    async fn get_all<ST, T, P>(table: TableReference, type_name: &str, query: String, filter: P) -> Result<Vec<T>, APIError>
    where
        ST: Serialize + DeserializeOwned + Clone,
        P: Fn(&ST) -> bool,
        T: From<ST>
    {
        let entries: Vec<ST> = TableStorage::get_all_entities(table, type_name, query, filter).await?;
        Ok(entries.iter().map(|e| e.clone().into()).collect())
    }

    #[instrument(err, skip( table, filter, not_found_err), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "LIST", db.statement = %query))]
    async fn get_random<ST, T, P>(table: TableReference, type_name: &str, query: String, filter: P, not_found_err: APIError) -> Result<T, APIError>
    where
        ST: Serialize + DeserializeOwned + Clone,
        P: Fn(&ST) -> bool,
        T: From<ST> + ToOwned
    {
        let entries: Vec<ST> = TableStorage::get_all_entities(table, type_name, query, filter).await?;
        entries.iter().choose(&mut rand::thread_rng()).map(|e| e).map(|e| e.clone().into()).ok_or(not_found_err)
    }

    #[instrument(err, skip( table, item), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "PUT"))]
    async fn store_single<ST, T>(table: TableReference, type_name: &str, item: ST) -> Result<T, APIError> 
    where
        ST: Serialize + DeserializeOwned + Clone + Debug,
        T: From<ST> {
        
        let result = table.insert().return_entity(true).execute(&item).await
        .map_err(|err| {
            error!("Failed to store item in table storage: {}", err);
            APIError::new(503, "Service Unavailable", "We were unable to store the item you requested, this failure has been reported.")
        })?;

        let entity = result.entity_with_metadata.map(|e| e.entity).unwrap();

        Ok(entity.into())
    }

    #[instrument(err, skip( table), fields(otel.kind = "client", db.system = "azure_table_storage", db.operation = "DELETE"))]
    async fn remove_single(table: TableReference, type_name: &str, partition_key: u128, row_key: u128) -> Result<(), APIError> {
        let entity_client = table
            .as_partition_key_client(&format!("{:0>32x}", partition_key))
            .as_entity_client(&format!("{:0>32x}", row_key))
            .map_err(|err| {
                error!("Failed to remove item from table storage: {}", err);
                APIError::new(500, "Internal Server Error", "We were unable to remove the item you requested, this failure has been reported.")
            })?;
            
        entity_client.delete().execute().await
        .map_err(|err| {
            error!("Failed to remove item from table storage: {}", err);
            APIError::new(503, "Service Unavailable", "We were unable to remove the item you requested, this failure has been reported.")
        })?;
        
        Ok(())
    }

    fn build_idea_filter_query(partition_key: u128, is_completed: Option<bool>, tag: Option<String>) -> String {
        let mut query = format!("$filter=PartitionKey eq '{:0>32x}'", partition_key);
        match is_completed {
            Some(completed) => {
                query = query + format!(" and Completed eq {}", completed).as_str()
            },
            None => {}
        }
        
        match tag {
            Some(tag) => {
                query = query + format!(" and contains(Tags, '{}')", tag.replace("'", "''").replace("%", "%25")).as_str()
            },
            None => {}
        }

        query
    }

    fn escape_query(query: &str) -> String {
        percent_encoding::percent_encode(query.as_bytes(), URI_CHARACTERS).to_string()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TableStorageIdea {
    #[serde(rename="PartitionKey")]
    pub collection_id: String,
    #[serde(rename="RowKey")]
    pub id: String,

    #[serde(rename="Name")]
    pub name: String,
    #[serde(rename="Description")]
    pub description: String,
    #[serde(rename="Tags")]
    pub tags: String,
    #[serde(rename="Completed")]
    pub completed: bool,
}

impl From<TableStorageIdea> for Idea {
    fn from(entity: TableStorageIdea) -> Self {
        Self {
            id: u128::from_str_radix(&entity.id, 16).unwrap_or_default(),
            collection_id: u128::from_str_radix(&entity.collection_id, 16).unwrap_or_default(),
            name: entity.name.clone(),
            tags: hashset!([entity.tags.split(",").filter(|t| !t.is_empty())]),
            description: entity.description.clone(),
            completed: entity.completed
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TableStorageCollection {
    #[serde(rename="PartitionKey")]
    pub principal_id: String,
    #[serde(rename="RowKey")]
    pub collection_id: String,

    #[serde(rename="Name")]
    pub name: String,
}

impl From<TableStorageCollection> for Collection {
    fn from(entity: TableStorageCollection) -> Self {
        Self {
            collection_id: u128::from_str_radix(&entity.collection_id, 16).unwrap_or_default(),
            user_id: u128::from_str_radix(&entity.principal_id, 16).unwrap_or_default(),
            name: entity.name.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TableStorageRoleAssignment {
    #[serde(rename="PartitionKey")]
    pub collection_id: String,
    #[serde(rename="RowKey")]
    pub principal_id: String,

    #[serde(rename="Role")]
    pub role: String,
}

impl From<TableStorageRoleAssignment> for RoleAssignment {
    fn from(entity: TableStorageRoleAssignment) -> Self {
        Self {
            collection_id: u128::from_str_radix(&entity.collection_id, 16).unwrap_or_default(),
            user_id: u128::from_str_radix(&entity.principal_id, 16).unwrap_or_default(),
            role: entity.role.as_str().into(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TableStorageUser {
    #[serde(rename="PartitionKey")]
    pub email_hash: String,

    #[serde(rename="RowKey")]
    pub row_key: String,

    #[serde(rename="PrincipalId")]
    pub principal_id: String,

    #[serde(rename="FirstName")]
    pub first_name: String,
}

impl From<TableStorageUser> for User {
    fn from(entity: TableStorageUser) -> Self {
        Self {
            email_hash: u128::from_str_radix(&entity.email_hash, 16).unwrap_or_default(),
            principal_id: u128::from_str_radix(&entity.principal_id, 16).unwrap_or_default(),
            first_name: entity.first_name.as_str().into(),
        }
    }
}

trait AsyncHandler<M>
where
    M: Message,
{
    type Result;

    // This method is called for every message received by this actor.
    fn handle_internal(&self, msg: M) -> Pin<Box<dyn Future<Output = Self::Result>>>;
}

macro_rules! actor_handler {
    ($msg:ty => $res:ty: handler = $handler:item) => {
        
        impl AsyncHandler<$msg> for TableStorage {
            type Result = Result<$res, APIError>;
            
            $handler
        }

        impl actix::Handler<$msg> for TableStorage {
            type Result = ResponseActFuture<Self, Result<$res, APIError>>;

            fn handle(&mut self, msg: $msg, _ctx: &mut Self::Context) -> Self::Result {
                Box::pin(fut::wrap_future(self.handle_internal(msg)))
            }
        }

        impl actix::Handler<$crate::telemetry::TraceMessage<$msg>> for TableStorage {
            type Result = ResponseActFuture<Self, Result<$res, APIError>>;

            fn handle(&mut self, msg: $crate::telemetry::TraceMessage<$msg>, _ctx: &mut Self::Context) -> Self::Result {
                let work = self.handle_internal(msg.message);

                let instrumentation = async move {
                    work.await
                }.instrument(msg.span);

                Box::pin(fut::wrap_future(instrumentation))
            }
        }
    };

    ($msg:ty|$src:ident => $res:ty: get_single from $table:ident ( $st:ty ) where pk=$pk:expr, rk=$rk:expr; not found = $err:expr) => {
        actor_handler!($msg => $res: handler = fn handle_internal(&self, $src: $msg) -> Pin<Box<dyn Future<Output = Self::Result>>> {
            let table = self.$table.clone();
            let work = TableStorage::get_single::<$st, $res>(
                table,
                "$table",
                $pk,
                $rk,
                APIError::new(404, "Not Found", $err));

            Box::pin(work)
        });
    };

    ($msg:ty|$src:ident => $res:ty: get_all from $table:ident ( $st:ty ) where query = $query:expr, context = [$($ctx:tt)*], filter = $fid:ident -> $filter:expr) => {
        actor_handler!($msg => Vec<$res>: handler = fn handle_internal(&self, $src: $msg) -> Pin<Box<dyn Future<Output = Self::Result>>> {
            let table = self.$table.clone();
            let query = $query;

            $($ctx)*

            let work = TableStorage::get_all::<$st, $res, _>(
                table,
                "$table",
                query,
                move |$fid| $filter
            );

            Box::pin(work)
        });
    };

    ($msg:ty|$src:ident => $res:ty: get_random from $table:ident ( $st:ty ) where query = $query:expr, context = [$($ctx:tt)*], filter = $fid:ident -> $filter:expr; not found = $err:expr) => {
        actor_handler!($msg => $res: handler = fn handle_internal(&self, $src: $msg) -> Pin<Box<dyn Future<Output = Self::Result>>> {
            let table = self.$table.clone();
            let query = $query;

            $($ctx)*

            let work = TableStorage::get_random::<$st, $res, _>(
                table,
                "$table",
                query,
                move |$fid| $filter,
                APIError::new(404, "Not Found", $err)
            );

            Box::pin(work)
        });
    };

    ($msg:ty|$src:ident: remove_single from $table:ident where pk=$pk:expr, rk=$rk:expr) => {
        actor_handler!($msg => (): handler = fn handle_internal(&self, $src: $msg) -> Pin<Box<dyn Future<Output = Self::Result>>> {
            let table = self.$table.clone();
            let work = TableStorage::remove_single(
                table,
                "$table",
                $pk,
                $rk);

            Box::pin(work)
        });
    };
    
    ($msg:ty|$src:ident => $res:ty: store_single in $table:ident ( $st:ty ) $item:expr) => {
        actor_handler!($msg => $res: handler = fn handle_internal(&self, $src: $msg) -> Pin<Box<dyn Future<Output = Self::Result>>> {
            let table = self.$table.clone();
            let item = $item;
            let work = TableStorage::store_single::<$st, $res>(
                table,
                "$table",
                item
            );

            Box::pin(work)
        });
    };
}

impl Actor for TableStorage {
    type Context = Context<Self>;
}

trace_handler!(TableStorage, GetHealth, Result<Health, APIError>);

impl Handler<GetHealth> for TableStorage {
    type Result = Result<Health, APIError>;

    fn handle(&mut self, _: GetHealth, _: &mut Self::Context) -> Self::Result {
        Ok(Health {
            ok: true,
            started_at: self.started_at.clone(),
        })
    }
}

actor_handler!(GetIdea|msg => Idea: get_single from ideas(TableStorageIdea) where pk=msg.collection, rk=msg.id; not found = "The combination of collection and idea ID you provided could not be found. Please check them and try again.");

actor_handler!(GetIdeas|msg => Idea: get_all from ideas(TableStorageIdea) where
    query=TableStorage::build_idea_filter_query(msg.collection, msg.is_completed, msg.tag.clone()),
    context = [
        let tag_str = msg.tag.unwrap_or("".to_string());
    ],
    filter=i -> tag_str.is_empty() || i.tags.split(",").any(|i| i == tag_str.as_str()));

    
actor_handler!(GetRandomIdea|msg => Idea: get_random from ideas(TableStorageIdea) where
    query = TableStorage::build_idea_filter_query(msg.collection, msg.is_completed, msg.tag.clone()),
    context = [
        let tag_str = msg.tag.unwrap_or("".to_string());
    ],
    filter = i -> tag_str.is_empty() || i.tags.split(",").any(|i| i == tag_str.as_str());
    not found = "We could not find any ideas in the collection you provided which matched your query. Please create some and try again.");

actor_handler!(StoreIdea|msg => Idea: store_single in ideas(TableStorageIdea) TableStorageIdea {
    collection_id: format!("{:0>32x}", msg.collection),
    id: format!("{:0>32x}", msg.id),
    name: msg.name.clone(),
    description: msg.description.clone(),
    tags: msg.tags.iter().fold("".to_string(), |j, i| j + "," + i.as_str()),
    completed: msg.completed,
});

actor_handler!(RemoveIdea|msg: remove_single from ideas where pk=msg.collection, rk=msg.id);

actor_handler!(GetCollection|msg => Collection: get_single from collections(TableStorageCollection) where pk=msg.principal_id, rk=msg.id; not found = "The collection ID you provided could not be found. Please check them and try again.");

actor_handler!(GetCollections|msg => Collection: get_all from collections(TableStorageCollection) where
    query = format!("$filter=PartitionKey eq '{:0>32x}'", msg.principal_id),
    context = [],
    filter = _i -> true);

actor_handler!(StoreCollection|msg => Collection: store_single in collections(TableStorageCollection) TableStorageCollection {
    principal_id: format!("{:0>32x}", msg.principal_id),
    collection_id: format!("{:0>32x}", msg.collection_id),
    name: msg.name.clone(),
});

actor_handler!(RemoveCollection|msg: remove_single from collections where pk=msg.principal_id, rk=msg.id);

actor_handler!(GetRoleAssignment|msg => RoleAssignment: get_single from role_assignments(TableStorageRoleAssignment) where pk=msg.collection_id, rk=msg.principal_id; not found = "The collection ID you provided could not be found. Please check them and try again.");

actor_handler!(GetRoleAssignments|msg => RoleAssignment: get_all from role_assignments(TableStorageRoleAssignment) where
    query = format!("$filter=PartitionKey eq '{:0>32x}'", msg.collection_id),
    context = [],
    filter = _i -> true);

actor_handler!(StoreRoleAssignment|msg => RoleAssignment: store_single in role_assignments(TableStorageRoleAssignment) TableStorageRoleAssignment {
    collection_id: format!("{:0>32x}", msg.collection_id),
    principal_id: format!("{:0>32x}", msg.principal_id),
    role: msg.role.into(),
}); 

actor_handler!(RemoveRoleAssignment|msg: remove_single from role_assignments where pk=msg.collection_id, rk=msg.principal_id);

actor_handler!(GetUser|msg => User: get_single from users(TableStorageUser) where pk=msg.email_hash, rk=msg.email_hash; not found = "The user you are looking for could not be found. Please check that you have entered their email address correctly and try again.");

actor_handler!(StoreUser|msg => User: store_single in users(TableStorageUser) TableStorageUser {
    email_hash: format!("{:0>32x}", msg.email_hash),
    row_key: format!("{:0>32x}", msg.email_hash),
    principal_id: format!("{:0>32x}", msg.principal_id),
    first_name: msg.first_name.clone()
});