use whatsapp_rust::prelude::SqliteStore;

const ADDRESS: &str = "15551212001.0:1@s.whatsapp.net";

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("db path");
    let mode = std::env::args().nth(2).unwrap_or_else(|| "open".into());
    match mode.as_str() {
        "open" => {
            let store = SqliteStore::new(&path).await.expect("migrated store opens");
            let loaded = store
                .load_identity_for_device(ADDRESS, 1)
                .await
                .expect("identity reads")
                .expect("identity survived");
            assert_eq!(loaded, vec![7u8; 32]);
            store
                .put_identity_for_device(ADDRESS, [8u8; 32], 1)
                .await
                .expect("rewrite after migration");
            let again = store
                .load_identity_for_device(ADDRESS, 1)
                .await
                .expect("reread")
                .expect("row");
            assert_eq!(again, vec![8u8; 32]);
            println!("reopened {path}");
        }
        "fresh" => {
            let store = SqliteStore::new(&path).await.expect("new store opens");
            store
                .put_identity_for_device("fresh.0:1@s.whatsapp.net", [9u8; 32], 1)
                .await
                .expect("fresh identity");
            let loaded = store
                .load_identity_for_device("fresh.0:1@s.whatsapp.net", 1)
                .await
                .expect("fresh read")
                .expect("fresh row");
            assert_eq!(loaded, vec![9u8; 32]);
            println!("fresh {path}");
        }
        "broken" => {
            match SqliteStore::new(&path).await {
                Ok(_) => {
                    eprintln!("broken store opened; migration did not fail");
                    std::process::exit(2);
                }
                Err(error) => {
                    println!("migration failed: {error}");
                }
            }
        }
        other => panic!("unknown mode {other}"),
    }
}
