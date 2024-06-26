use anyhow::Result;
use futures::SinkExt;
use selium::prelude::*;
use selium::std::codecs::BincodeCodec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct StockEvent {
    ticker: String,
    change: f64,
}

impl StockEvent {
    pub fn new(ticker: &str, change: f64) -> Self {
        Self {
            ticker: ticker.to_owned(),
            change,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let connection = selium::custom()
        .keep_alive(5_000)?
        .endpoint("127.0.0.1:7001")
        .with_certificate_authority("../certs/client/ca.der")?
        .with_cert_and_key(
            "../certs/client/localhost.der",
            "../certs/client/localhost.key.der",
        )?
        .connect()
        .await?;

    let mut publisher = connection
        .publisher("/acmeco/stocks")
        .with_encoder(BincodeCodec::default())
        .open()
        .await?;

    tokio::spawn({
        let mut publisher = publisher.duplicate().await.unwrap();
        async move {
            publisher
                .send(StockEvent::new("MSFT", 12.75))
                .await
                .unwrap();

            publisher.finish().await.unwrap();
        }
    });

    publisher.send(StockEvent::new("APPL", 3.5)).await?;
    publisher.send(StockEvent::new("INTC", -9.0)).await?;
    publisher.finish().await?;

    Ok(())
}
