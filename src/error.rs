//! The error type for operations that mix modules. Each module has its own
//! error enum; `Error` wraps them so that a frontend matches on one type when
//! an operation in `ops` can fail in more than one place. This is what the
//! future `aggrega_core` crate will export as its error type.

use crate::{db, fetch, opml, sync};

/// Why an operation failed, from whichever module it was.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    #[error(transparent)]
    Opml(#[from] opml::Error),
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    Sync(#[from] sync::Error),
}

impl Error {
    /// The server or the other computer couldn't be reached at all, which says nothing about it.
    #[allow(dead_code)] // Not called outside the tests yet; frontends will match on it.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Fetch(fetch::Error::Unreachable { .. })
                | Self::Sync(sync::Error::Unreachable { .. })
        )
    }
}

/// `err` and its sources joined by ": ", for logs (what anyhow's `{:#}` printed).
pub fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    for source in std::iter::successors(err.source(), |e| e.source()) {
        let message = source.to_string();
        // Some errors (feed-rs's) already end with their source's message.
        if !text.ends_with(&message) {
            text += ": ";
            text += &message;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Propagates `err` with `?`, as an operation does.
    fn lift<E>(err: E) -> Result<(), Error>
    where
        Error: From<E>,
    {
        Err::<(), E>(err)?;
        Ok(())
    }

    #[test]
    fn question_mark_lifts_each_module_error_into_its_variant() {
        assert!(matches!(
            lift(fetch::Error::EmptyAddress).unwrap_err(),
            Error::Fetch(fetch::Error::EmptyAddress)
        ));
        assert!(matches!(
            lift(opml::Error::NotOpml).unwrap_err(),
            Error::Opml(opml::Error::NotOpml)
        ));
        assert!(matches!(
            lift(db::Error::NotAnAggregaDatabase).unwrap_err(),
            Error::Db(db::Error::NotAnAggregaDatabase)
        ));
        assert!(matches!(
            lift(sync::Error::EmptyAddress).unwrap_err(),
            Error::Sync(sync::Error::EmptyAddress)
        ));
    }

    #[test]
    fn a_server_or_computer_that_cant_be_reached_is_unreachable() {
        let server: Error = fetch::Error::Unreachable {
            url: "https://example.org/feed.xml".into(),
            source: ureq::Error::HostNotFound,
        }
        .into();
        let computer: Error = sync::Error::Unreachable {
            addr: "192.168.1.5:7000".parse().unwrap(),
            source: io::Error::from(io::ErrorKind::ConnectionRefused),
        }
        .into();

        assert!(server.is_unreachable());
        assert!(computer.is_unreachable());
    }

    #[test]
    fn failures_after_connecting_are_not_unreachable() {
        let errors: [Error; 4] = [
            fetch::Error::Status {
                url: "https://example.org/feed.xml".into(),
                status: 404,
            }
            .into(),
            sync::Error::Connection {
                addr: "192.168.1.5:7000".parse().unwrap(),
                source: io::Error::from(io::ErrorKind::ConnectionReset),
            }
            .into(),
            db::Error::NotAnAggregaDatabase.into(),
            opml::Error::NotOpml.into(),
        ];

        for err in &errors {
            assert!(!err.is_unreachable(), "{err}");
        }
    }

    #[test]
    fn chain_joins_an_error_and_its_source() {
        let err = fetch::Error::Unreachable {
            url: "http://127.0.0.1:9/feed.xml".into(),
            source: ureq::Error::HostNotFound,
        };

        assert_eq!(
            chain(&err),
            format!(
                "couldn't reach http://127.0.0.1:9/feed.xml: {}",
                ureq::Error::HostNotFound
            )
        );
    }

    #[test]
    fn chain_skips_a_source_its_error_already_quotes() {
        // feed-rs's message ends with serde_json's, which is also its source.
        let err = crate::feed::parse("https://x.org/feed.json", b"{\"version\": 1").unwrap_err();
        let feed_rs = std::error::Error::source(&err).unwrap();

        assert_eq!(chain(&err), format!("{err}: {feed_rs}"));
    }

    #[test]
    fn chain_of_an_error_without_a_source_is_its_message() {
        assert_eq!(
            chain(&db::Error::NotAnAggregaDatabase),
            "not an Aggrega database"
        );
    }
}
