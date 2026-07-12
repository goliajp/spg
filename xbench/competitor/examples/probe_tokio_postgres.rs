//! Ground truth for the binary result-format work: tokio-postgres
//! (which requests binary results by default) against a live SPGS.
//! Point it at SPG with TPG_URL, e.g.
//!   TPG_URL=postgres://u:p@127.0.0.1:6033/app cargo run --example probe_tokio_postgres

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("TPG_URL")
        .unwrap_or_else(|_| "postgres://u:p@127.0.0.1:6033/app".into());
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("conn task error: {e:?}");
        }
    });
    eprintln!("connected");
    client
        .execute("CREATE TABLE tp (i INT, b BIGINT, t TEXT, f FLOAT, ok BOOLEAN)", &[])
        .await?;
    eprintln!("create ok");
    client
        .execute(
            "INSERT INTO tp VALUES (7, 300000000000, 'hi', 1.5, true)",
            &[],
        )
        .await?;
    eprintln!("insert ok");
    let row = client
        .query_one("SELECT i, b, t, f, ok FROM tp", &[])
        .await?;
    let (i, b, t, f, ok): (i32, i64, String, f64, bool) =
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
    assert_eq!((i, b, t.as_str(), f, ok), (7, 300000000000, "hi", 1.5, true));
    // Parameterised query exercises binary Bind params too.
    let row = client
        .query_one("SELECT i FROM tp WHERE i = $1", &[&7i32])
        .await?;
    let got: i32 = row.get(0);
    assert_eq!(got, 7);
    println!("tokio-postgres binary round-trip OK");
    Ok(())
}
