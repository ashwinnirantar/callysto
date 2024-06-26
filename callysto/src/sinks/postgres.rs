use crate::errors::*;
use crate::runtime::async_con::Arc;
use crossbeam_channel::{Receiver, Sender};
use cuneiform_fields::arch::ArchPadding;
use deadpool_postgres::tokio_postgres::types::ToSql;
use deadpool_postgres::{Client, Config, Pool, PoolError, Runtime};
use deadpool_postgres::{Manager, ManagerConfig, RecyclingMethod};
use futures::sink::{drain, With};
use futures::Future;
use futures::{Sink, SinkExt};
use futures_lite::FutureExt;
use nuclei::Task;
use pin_project_lite::pin_project;
use serde::Serialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::vec::Drain;
use tracing::{debug, error, info, trace, warn};
use url::Url;

#[derive(Debug)]
pub struct CPostgresRow<T: ToSql + Sync + 'static + Send> {
    pub query: String,
    pub args: Vec<T>,
}

impl<T> CPostgresRow<T>
where
    T: Send + ToSql + Sync + 'static,
{
    pub fn new(query: String, args: Vec<T>) -> Self {
        Self { query, args }
    }
}

pin_project! {
    pub struct CPostgresSink<T>
    where
    T: ToSql,
    T: Sync,
    T: Send,
    T: 'static
    {
        client: Arc<deadpool_postgres::Pool>,
        tx: ArchPadding<Sender<CPostgresRow<T>>>,
        buffer_size: usize,
        #[pin]
        data_sink: Task<()>
    }
}

impl<T> CPostgresSink<T>
where
    T: ToSql + Sync + 'static + Send,
{
    async fn setup_pg(dsn: &str, tls: bool, pool_size: usize) -> Result<Pool> {
        // TODO(ansrivas): Currently only NoTls is supported, will add it later.
        let pg_config = deadpool_postgres::tokio_postgres::Config::from_str(dsn)?;
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };

        let mgr = Manager::from_config(
            pg_config,
            deadpool_postgres::tokio_postgres::NoTls,
            mgr_config,
        );

        info!("attempting database connection");

        let pool_result = Pool::builder(mgr).max_size(pool_size).build();

        let pool = match pool_result {
            Ok(pool) => pool,
            Err(e) => return Err(CallystoError::GeneralError(e.to_string())),
        };

        info!("no error in creating a connection pool to the database");

        Ok(pool)
    }

    pub fn new<S: Into<String>>(pg_dsn: S, pool_size: usize, buffer_size: usize) -> Result<Self> {
        let pgpool = nuclei::block_on(async move {
            Self::setup_pg(&pg_dsn.into(), false, pool_size)
                .await
                .unwrap_or_else(|err| panic!("Error connecting to the database: {}", err))
        });

        info!("using clone version of postgres library sink v1.10.2");

        let (tx, rx) = crossbeam_channel::unbounded::<CPostgresRow<T>>();
        let (tx, rx) = (ArchPadding::new(tx), ArchPadding::new(rx));

        let inner_client = pgpool.clone();
        info!("cloned pgpool successfully");

        let client = Arc::new(pgpool);
        info!("created pointer to pgpool");

        let data_sink = nuclei::spawn(async move {
            match nuclei::spawn_more_threads(1).await {
                Ok(_) => {info!("created threads")}
                Err(_) => {} //log max thread issues?
            };
            info!("creating a new nuclei spawn");
            let mut loop_entered = false; // Flag to track if the loop is entered
            while let Ok(item) = rx.recv() {
                loop_entered = true; // Set the flag to true when the loop is entered
                let mut client = inner_client
                    .get()
                    .await
                    .unwrap_or_else(|err| panic!("Error preparing client: {}", err));
                info!("prepared client");
                let stmt = client
                    .prepare_cached(&item.query)
                    .await
                    .unwrap_or_else(|err| panic!("Error preparing statement: {}", err));
                info!("prepared statement");
                let rows = client
                    .query_raw(&stmt, &item.args)
                    .await
                    .unwrap_or_else(|err| panic!("Error executing statement: {}", err));
                info!("querying row");
                info!("CPostgresSink - Ingestion status:");
            }
            if !loop_entered {
                info!("The while loop was not entered");
            } else {
                info!("while loop was entered")
            }
        });

        Ok(Self {
            client,
            tx,
            buffer_size,
            data_sink,
        })
    }
}

impl<T> Sink<CPostgresRow<T>> for CPostgresSink<T>
where
    T: ToSql + Sync + 'static + Send,
{
    type Error = CallystoError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.buffer_size == 0 {
            // Bypass buffering
            return Poll::Ready(Ok(()));
        }

        if self.tx.len() >= self.buffer_size {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: CPostgresRow<T>) -> Result<()> {
        let mut this = &mut *self;
        this.tx
            .send(item)
            .map_err(|e| CallystoError::GeneralError(format!("Failed to send to db: `{}`", e)))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.tx.len() > 0 {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.tx.len() > 0 {
            Poll::Pending
        } else {
            // TODO: Drop the task `data_sink`.
            Poll::Ready(Ok(()))
        }
    }
}
