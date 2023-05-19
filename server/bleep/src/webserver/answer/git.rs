use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use gix::{actor::SignatureRef, bstr::ByteSlice, object::tree::diff::Action};

use crate::repo::RepoRef;

use super::AppContext;

pub(super) struct LogSearch {
    pub(super) query: Option<String>,
    pub(super) author: Option<String>,
    pub(super) start_date: Option<DateTime<Utc>>,
    pub(super) end_date: Option<DateTime<Utc>>,
    pub(super) file: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(super) struct LogSearchResult {
    hash: String,
    author: String,
    committer: String,
    message: String,
    time: String,
}

impl LogSearch {
    pub(super) async fn run(
        self,
        ctx: &AppContext,
        reporef: &RepoRef,
    ) -> Result<Vec<LogSearchResult>> {
        let mut git = ctx
            .app
            .repo_pool
            .read_async(reporef, |_k, v| v.git())
            .await
            .context("invalid repo")??;

        // I suspect this will impact the number of parallel open
        // files, so keep it sane.
        git.object_cache_size(Some(128));

        // offset is 0 since we take `Utc` to start with
        let parse_date =
            |date: DateTime<Utc>| gix::date::Time::new(date.timestamp().try_into().unwrap(), 0);

        let check_author = |query: &str, author: SignatureRef<'_>| {
            let (name, email) = author.actor();
            name.to_str_lossy().to_lowercase().contains(query)
                || email.to_str_lossy().to_lowercase().contains(query)
        };

        let mut start_date = self.start_date.map(parse_date);
        let mut end_date = self.end_date.map(parse_date);

        // enforce ordering of the range
        if end_date < start_date {
            (start_date, end_date) = (end_date, start_date);
        }

        let head = || Ok::<_, anyhow::Error>(git.head()?.peel_to_commit_in_place()?);
        let commits = head()?
            .ancestors()
            .all()?
            .map(|id| id.unwrap().object().unwrap().into_commit())
            // we're going through the list in reverse chronological
            // order, so apply filters accordingly
            .skip_while(|commit| match end_date {
                Some(date) => commit.time().unwrap() > date,
                None => false,
            })
            .take_while(|commit| match start_date {
                Some(date) => commit.time().unwrap() > date,
                None => true,
            })
            // we implement an AND logic here
            .filter_map(|commit| {
                let mut decision = match self.author {
                    None => true,
                    Some(ref q) => {
                        let query = q.to_lowercase();
                        check_author(query.as_ref(), commit.author().unwrap())
                            | check_author(query.as_ref(), commit.committer().unwrap())
                    }
                };

                decision = match self.query {
                    None => decision,
                    Some(ref q) => commit
                        .message_raw_sloppy()
                        .to_str_lossy()
                        .to_lowercase()
                        .contains(q.to_lowercase().as_str()),
                };

                decision = match self.file {
                    None => decision,
                    Some(ref f) => {
                        let parent = commit.ancestors().first_parent_only().all().unwrap().next();
                        let parent_tree = match parent {
                            Some(Ok(id)) => id.object().unwrap().into_commit().tree().unwrap(),
                            _ => git.empty_tree(),
                        };

                        let mut decision = false;
                        _ = commit
                            .tree()
                            .unwrap()
                            .changes()
                            .unwrap()
                            .track_path()
                            .for_each_to_obtain_tree(&parent_tree, |change| {
                                if change.location.contains_str(f) {
                                    decision = true;
                                    Ok::<_, VoidError>(Action::Cancel)
                                } else {
                                    Ok(Action::Continue)
                                }
                            });

                        decision
                    }
                };

                let to_author_string = |(name, email)| format!("{name} <{email}>");
                let time = commit.time().unwrap();
                if decision {
                    Some(LogSearchResult {
                        hash: commit.id.to_string(),
                        author: to_author_string(commit.author().unwrap().actor()),
                        committer: to_author_string(commit.committer().unwrap().actor()),
                        message: commit.message().unwrap().summary().to_string(),
                        time: FixedOffset::east_opt(time.offset_in_seconds)
                            .unwrap()
                            .timestamp_opt(time.seconds_since_unix_epoch.into(), 0)
                            .unwrap()
                            .to_string(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok(commits)
    }
}

#[derive(thiserror::Error, Debug)]
#[error("never happens")]
struct VoidError;
