use whatsapp_rust::prelude::SqliteStore;

const ADDRESS: &str = "15551212001.0:1@s.whatsapp.net";

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("db path");
    let store = SqliteStore::new(&path).await.expect("old store opens");
    store
        .put_identity_for_device(ADDRESS, [7u8; 32], 1)
        .await
        .expect("identity is stored");
    let loaded = store
        .load_identity_for_device(ADDRESS, 1)
        .await
        .expect("identity reads")
        .expect("identity row");
    assert_eq!(loaded, vec![7u8; 32]);
    println!("created {path}");
}
