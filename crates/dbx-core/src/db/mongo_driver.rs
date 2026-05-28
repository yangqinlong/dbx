use mongodb::{
    bson::{doc, Bson, Document},
    Client,
};
use serde::{Deserialize, Serialize};

use super::with_connection_timeout;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDocumentResult {
    pub documents: Vec<serde_json::Value>,
    pub total: u64,
}

pub async fn connect(url: &str, timeout: Duration) -> Result<Client, String> {
    with_connection_timeout("MongoDB", timeout, async {
        Client::with_uri_str(url).await.map_err(|e| format!("MongoDB connection failed: {e}"))
    })
    .await
}

pub async fn test_connection(client: &Client, timeout: Duration) -> Result<(), String> {
    tokio::time::timeout(timeout, client.list_database_names())
        .await
        .map_err(|_| format!("MongoDB connection timed out ({}s)", timeout.as_secs()))?
        .map(|_| ())
        .map_err(|e| format!("MongoDB connection failed: {e}"))
}

pub async fn list_databases(client: &Client) -> Result<Vec<String>, String> {
    client.list_database_names().await.map_err(|e| e.to_string())
}

pub async fn list_collections(client: &Client, database: &str) -> Result<Vec<String>, String> {
    client.database(database).list_collection_names().await.map_err(|e| e.to_string())
}

pub async fn find_documents(
    client: &Client,
    database: &str,
    collection: &str,
    skip: u64,
    limit: i64,
    filter: Option<&str>,
    sort: Option<&str>,
) -> Result<MongoDocumentResult, String> {
    let col = client.database(database).collection::<Document>(collection);

    let filter_doc: Document = match filter {
        Some(f) if !f.trim().is_empty() => {
            let json: serde_json::Value = serde_json::from_str(f).map_err(|e| format!("Invalid filter JSON: {e}"))?;
            mongodb::bson::to_document(&json).map_err(|e| format!("Invalid filter: {e}"))?
        }
        _ => doc! {},
    };

    let total = col.count_documents(filter_doc.clone()).await.map_err(|e| e.to_string())?;

    let mut find = col.find(filter_doc).skip(skip).limit(limit);
    if let Some(s) = sort {
        if !s.trim().is_empty() {
            let json: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("Invalid sort JSON: {e}"))?;
            let sort_doc = mongodb::bson::to_document(&json).map_err(|e| format!("Invalid sort: {e}"))?;
            find = find.sort(sort_doc);
        }
    }

    let mut cursor = find.await.map_err(|e| e.to_string())?;

    let mut documents = Vec::new();
    while cursor.advance().await.map_err(|e| e.to_string())? {
        let doc = cursor.deserialize_current().map_err(|e| e.to_string())?;
        let json = bson_to_json(&Bson::Document(doc));
        documents.push(json);
    }

    Ok(MongoDocumentResult { documents, total })
}

pub async fn insert_document(
    client: &Client,
    database: &str,
    collection: &str,
    doc_json: &str,
) -> Result<String, String> {
    let doc: Document = serde_json::from_str(doc_json).map_err(|e| format!("Invalid JSON: {e}"))?;
    let col = client.database(database).collection::<Document>(collection);
    let result = col.insert_one(doc).await.map_err(|e| e.to_string())?;
    Ok(format!("{}", result.inserted_id))
}

pub async fn update_document(
    client: &Client,
    database: &str,
    collection: &str,
    id: &str,
    doc_json: &str,
) -> Result<u64, String> {
    let oid = mongodb::bson::oid::ObjectId::parse_str(id).map_err(|e| format!("Invalid ObjectId: {e}"))?;
    let mut new_doc: Document = serde_json::from_str(doc_json).map_err(|e| format!("Invalid JSON: {e}"))?;
    new_doc.remove("_id");
    let col = client.database(database).collection::<Document>(collection);
    let result = col.replace_one(doc! { "_id": oid }, new_doc).await.map_err(|e| e.to_string())?;
    Ok(result.modified_count)
}

pub async fn delete_document(client: &Client, database: &str, collection: &str, id: &str) -> Result<u64, String> {
    let oid = mongodb::bson::oid::ObjectId::parse_str(id).map_err(|e| format!("Invalid ObjectId: {e}"))?;
    let col = client.database(database).collection::<Document>(collection);
    let result = col.delete_one(doc! { "_id": oid }).await.map_err(|e| e.to_string())?;
    Ok(result.deleted_count)
}

fn bson_to_json(bson: &Bson) -> serde_json::Value {
    match bson {
        Bson::Double(v) => serde_json::json!(v),
        Bson::String(v) => serde_json::Value::String(v.clone()),
        Bson::Boolean(v) => serde_json::Value::Bool(*v),
        Bson::Null => serde_json::Value::Null,
        Bson::Int32(v) => serde_json::json!(v),
        Bson::Int64(v) => serde_json::json!(v),
        Bson::ObjectId(oid) => serde_json::Value::String(oid.to_hex()),
        Bson::DateTime(dt) => serde_json::Value::String(dt.to_string()),
        Bson::Array(arr) => serde_json::Value::Array(arr.iter().map(bson_to_json).collect()),
        Bson::Document(doc) => {
            let mut map = serde_json::Map::new();
            for (k, v) in doc {
                map.insert(k.clone(), bson_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::String(format!("{bson}")),
    }
}
