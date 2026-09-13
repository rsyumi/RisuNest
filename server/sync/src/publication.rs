use crate::{store::Store, Error, Result};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::{watch, Notify};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationStatus {
    pub phase: &'static str,
    pub error: Option<&'static str>,
}

pub struct Publisher {
    http: reqwest::Client,
    status: watch::Sender<PublicationStatus>,
}
impl Publisher {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::new("directory-client-unavailable", 503))?;
        let (status, _) = watch::channel(PublicationStatus {
            phase: "waiting",
            error: None,
        });
        Ok(Self { http, status })
    }
    pub fn subscribe(&self) -> watch::Receiver<PublicationStatus> {
        self.status.subscribe()
    }
    pub async fn publish_once(&self, store: &Store) -> Result<bool> {
        let Some(publication) = store.plan_publication()? else {
            let status = store.connection_status()?;
            self.status.send_replace(PublicationStatus {
                phase: status.publication,
                error: None,
            });
            return Ok(false);
        };
        self.status.send_replace(PublicationStatus {
            phase: "publishing",
            error: None,
        });
        let response = self
            .http
            .post(publication.directory.record_url()?)
            .header("content-type", "text/plain; charset=utf-8")
            .header("accept-encoding", "identity")
            .body(publication.envelope.clone())
            .send()
            .await
            .map_err(|_| Error::new("directory-unreachable", 503))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(Error::new(
                "directory-publication-failed",
                response.status().as_u16(),
            ));
        }
        store.confirm_publication(&publication)?;
        self.status.send_replace(PublicationStatus {
            phase: "published",
            error: None,
        });
        Ok(true)
    }
    pub async fn run(
        self,
        store: Arc<Store>,
        changed: Arc<Notify>,
        mut stop: watch::Receiver<bool>,
        wait_for_address: bool,
    ) {
        if wait_for_address {
            tokio::select! { _ = changed.notified() => (), _ = stop.changed() => return }
        }
        let mut failures = 0u32;
        loop {
            if *stop.borrow() {
                return;
            }
            let outcome = tokio::select! {
                _ = stop.changed() => return,
                value = self.publish_once(&store) => value,
            };
            match outcome {
                Ok(_) => {
                    failures = 0;
                    // Check persisted renewal time without making an HTTP request
                    // until the seven-day interval has elapsed.
                    tokio::select! {
                        _ = changed.notified() => (),
                        _ = tokio::time::sleep(Duration::from_secs(60)) => (),
                        _ = stop.changed() => return,
                    }
                }
                Err(error) if error.code == "publication-superseded" => continue,
                Err(error) => {
                    self.status.send_replace(PublicationStatus {
                        phase: "failed",
                        error: Some(error.code),
                    });
                    failures = failures.saturating_add(1);
                    let delay = Duration::from_secs((1u64 << failures.min(6)).min(60));
                    tokio::select! { _ = changed.notified() => (), _ = tokio::time::sleep(delay) => (), _ = stop.changed() => return }
                }
            }
        }
    }
}
