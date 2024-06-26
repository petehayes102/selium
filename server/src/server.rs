use crate::args::{LogArgs, UserArgs};
use crate::quic::{load_root_store, read_certs, server_config, ConfigOptions};
use crate::topic::config::TopicConfig;
use crate::topic::{pubsub, reqrep, Sender, Socket};
use anyhow::{anyhow, bail, Context, Result};
use futures::{future::join_all, stream::FuturesUnordered, SinkExt, StreamExt};
use log::{error, info};
use quinn::{Connecting, Connection, Endpoint, IdleTimeout, VarInt};
use selium_log::config::{FlushPolicy, LogConfig};
use selium_log::MessageLog;
use selium_protocol::error_codes::INVALID_TOPIC_NAME;
use selium_protocol::{error_codes, BiStream, ErrorPayload, Frame, TopicName};
use std::net::SocketAddr;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::Mutex, task::JoinHandle};

pub(crate) type SharedTopics = Arc<Mutex<HashMap<TopicName, Sender>>>;
type SharedTopicHandles = Arc<Mutex<FuturesUnordered<JoinHandle<()>>>>;

pub struct Server {
    topics: SharedTopics,
    topic_handles: SharedTopicHandles,
    log_args: Arc<LogArgs>,
    endpoint: Endpoint,
}

impl Server {
    pub async fn listen(&self) -> Result<()> {
        loop {
            tokio::select! {
                Some(conn) = self.endpoint.accept() => {
                    self.connect(conn).await?;
                },
                Ok(()) = tokio::signal::ctrl_c() => {
                    self.shutdown().await?;
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn addr(&self) -> Result<SocketAddr> {
        let addr = self.endpoint.local_addr()?;
        Ok(addr)
    }

    async fn connect(&self, conn: Connecting) -> Result<()> {
        info!("connection incoming");
        let topics_clone = self.topics.clone();
        let topic_handles = self.topic_handles.clone();
        let log_args = self.log_args.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(topics_clone, topic_handles, conn, log_args).await {
                error!("connection failed: {:?}", e);
            }
        });

        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Shutdown signal received: preparing to gracefully shutdown.");
        self.endpoint.reject_new_connections();

        let mut topics = self.topics.lock().await;
        let mut topic_handles = self.topic_handles.lock().await;

        topics.values_mut().for_each(|t| t.close_channel());
        join_all(topic_handles.iter_mut()).await;

        self.endpoint.close(
            VarInt::from_u32(error_codes::SHUTDOWN),
            b"Scheduled shutdown.",
        );
        self.endpoint.wait_idle().await;

        Ok(())
    }
}

impl TryFrom<UserArgs> for Server {
    type Error = anyhow::Error;

    fn try_from(args: UserArgs) -> Result<Self, Self::Error> {
        let root_store = load_root_store(args.cert.ca)?;
        let (certs, key) = read_certs(args.cert.cert, args.cert.key)?;
        let log_args = Arc::new(args.log);

        let opts = ConfigOptions {
            keylog: args.keylog,
            stateless_retry: args.stateless_retry,
            max_idle_timeout: IdleTimeout::from(VarInt::from_u32(args.max_idle_timeout)),
        };

        let config = server_config(root_store, certs, key, opts)?;
        let endpoint = Endpoint::server(config, args.bind_addr)?;

        // Create hash to store message ordering data
        let topics = Arc::new(Mutex::new(HashMap::new()));
        let topic_handles = Arc::new(Mutex::new(FuturesUnordered::new()));

        Ok(Self {
            topics,
            topic_handles,
            log_args,
            endpoint,
        })
    }
}

async fn handle_connection(
    topics: SharedTopics,
    topic_handles: SharedTopicHandles,
    conn: quinn::Connecting,
    log_args: Arc<LogArgs>,
) -> Result<()> {
    let connection = conn.await?;
    info!(
        "Connection {} - {}",
        connection.remote_address(),
        connection
            .handshake_data()
            .unwrap()
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap()
            .protocol
            .map_or_else(
                || "<none>".into(),
                |x| String::from_utf8_lossy(&x).into_owned()
            )
    );

    loop {
        let connection = connection.clone();
        let stream = connection.accept_bi().await;
        let stream = match stream {
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                info!("Connection closed ({})", connection.remote_address());
                return Ok(());
            }
            Err(e) => {
                bail!(e)
            }
            Ok(stream) => BiStream::from(stream),
        };

        let topics_clone = topics.clone();
        let topic_handles_clone = topic_handles.clone();
        let log_args = log_args.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_stream(
                topics_clone,
                topic_handles_clone,
                stream,
                connection,
                log_args,
            )
            .await
            {
                error!("Request failed: {:?}", e);
            }
        });
    }
}

async fn handle_stream(
    topics: SharedTopics,
    topic_handles: SharedTopicHandles,
    mut stream: BiStream,
    _connection: Connection,
    log_args: Arc<LogArgs>,
) -> Result<()> {
    // Receive header
    if let Some(result) = stream.next().await {
        let frame = result?;
        let topic = frame.get_topic().ok_or(anyhow!("Expected header frame"))?;

        #[cfg(feature = "__cloud")]
        {
            use crate::cloud::do_cloud_auth;
            use log::debug;
            use selium_protocol::error_codes::CLOUD_AUTH_FAILED;

            match do_cloud_auth(&_connection, topic, &topics).await {
                Ok(_) => stream.send(Frame::Ok).await?,
                Err(e) => {
                    debug!("Cloud authentication error: {e:?}");

                    let payload = ErrorPayload {
                        code: CLOUD_AUTH_FAILED,
                        message: e.to_string().into(),
                    };

                    stream.send(Frame::Error(payload)).await?;

                    return Ok(());
                }
            }
        }
        #[cfg(not(feature = "__cloud"))]
        {
            // Note this can only occur if someone circumvents the client lib
            if !topic.is_valid() {
                let payload = ErrorPayload {
                    code: INVALID_TOPIC_NAME,
                    message: "Invalid topic name".into(),
                };
                stream.send(Frame::Error(payload)).await?;
                return Ok(());
            }
            stream.send(Frame::Ok).await?;
        }

        let mut ts = topics.lock().await;

        // Spawn new topic if it doesn't exist yet
        if !ts.contains_key(topic) {
            match frame {
                Frame::RegisterPublisher(_) | Frame::RegisterSubscriber(_) => {
                    let retention_period = frame.retention_policy().unwrap();
                    let topic_path = topic.to_string();
                    let segments_path = log_args
                        .log_segments_directory
                        .join(topic_path.trim_matches('/'));

                    let mut flush_policy = FlushPolicy::default()
                        .interval(Duration::from_millis(log_args.flush_policy_interval));

                    if let Some(num_writes) = log_args.flush_policy_num_writes {
                        flush_policy = flush_policy.number_of_writes(num_writes);
                    }

                    let topic_config = Arc::new(TopicConfig::new(Duration::from_millis(
                        log_args.subscriber_polling_interval,
                    )));

                    let log_config = Arc::new(
                        LogConfig::from_path(segments_path)
                            .max_index_entries(log_args.log_maximum_entries)
                            .retention_period(Duration::from_millis(retention_period))
                            .cleaner_interval(Duration::from_millis(log_args.log_cleaner_interval))
                            .flush_policy(flush_policy),
                    );

                    let log = MessageLog::open(log_config).await?;
                    let (mut fut, tx) = pubsub::Topic::pair(log, topic_config);

                    let handle = tokio::spawn(async move {
                        fut.run().await.unwrap();
                    });

                    topic_handles.lock().await.push(handle);
                    ts.insert(topic.clone(), Sender::Pubsub(tx));
                }
                Frame::RegisterReplier(_) | Frame::RegisterRequestor(_) => {
                    let (fut, tx) = reqrep::Topic::pair();
                    let handle = tokio::spawn(fut);

                    topic_handles.lock().await.push(handle);
                    ts.insert(topic.clone(), Sender::ReqRep(tx));
                }
                _ => unreachable!(), // because of `topic` instantiation
            };
        }

        let tx = ts.get_mut(topic).unwrap();

        match frame {
            Frame::RegisterPublisher(_) => {
                let (_, read) = stream.split();
                tx.send(Socket::Pubsub(pubsub::Socket::Stream(Box::pin(read))))
                    .await
                    .context("Failed to add Publisher stream")?;
            }
            Frame::RegisterSubscriber(payload) => {
                let (write, _) = stream.split();
                tx.send(Socket::Pubsub(pubsub::Socket::Sink(
                    Box::pin(write),
                    payload.offset,
                )))
                .await
                .context("Failed to add Subscriber sink")?;
            }
            Frame::RegisterReplier(_) => {
                let (si, st) = stream.split();
                tx.send(Socket::Reqrep(reqrep::Socket::Server((
                    Box::pin(si),
                    Box::pin(st),
                ))))
                .await
                .context("Failed to add Replier")?;
            }
            Frame::RegisterRequestor(_) => {
                let (si, st) = stream.split();
                tx.send(Socket::Reqrep(reqrep::Socket::Client((
                    Box::pin(si),
                    Box::pin(st),
                ))))
                .await
                .context("Failed to add Requestor")?;
            }
            _ => unreachable!(), // because of `topic` instantiation
        }
    } else {
        info!("Stream closed");
    }

    Ok(())
}
