use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use clap::Parser;
use derivative::Derivative;
use prettytable::{cell, format, row, Table};

use super::{progress_counter, prune};
use crate::backend::{DecryptFullBackend, FileType};
use crate::repo::{
    ConfigFile, SnapshotFile, SnapshotFilter, SnapshotGroup, SnapshotGroupCriterion, StringList,
};

#[derive(Parser)]
pub(super) struct Opts {
    #[clap(flatten)]
    filter: SnapshotFilter,

    /// group snapshots by any combination of host,paths,tags
    #[clap(
        long,
        short = 'g',
        value_name = "CRITERION",
        default_value = "host,paths"
    )]
    group_by: SnapshotGroupCriterion,

    #[clap(flatten)]
    keep: KeepOptions,

    /// also prune the repository
    #[clap(long)]
    prune: bool,

    #[clap(flatten)]
    prune_opts: prune::Opts,

    /// don't remove anything, only show what would be done
    #[clap(skip)]
    dry_run: bool,

    /// Snapshots to forget
    ids: Vec<String>,
}

pub(super) async fn execute(
    be: &(impl DecryptFullBackend + Unpin),
    mut opts: Opts,
    config: ConfigFile,
) -> Result<()> {
    opts.dry_run = opts.prune_opts.dry_run;
    let groups = match opts.ids.is_empty() {
        true => SnapshotFile::group_from_backend(be, &opts.filter, &opts.group_by).await?,
        false => vec![(
            SnapshotGroup::default(),
            SnapshotFile::from_ids(be, &opts.ids).await?,
        )],
    };
    let mut forget_snaps = Vec::new();

    for (group, mut snapshots) in groups {
        if !group.is_empty() {
            println!("snapshots for {:?}", group);
        }
        snapshots.sort_unstable_by(|sn1, sn2| sn1.cmp(sn2).reverse());
        let latest_time = snapshots[0].time;
        let mut group_keep = opts.keep.clone();
        let mut table = Table::new();

        let mut iter = snapshots.iter().peekable();
        let mut last = None;
        let now = Local::now();
        // snapshots that have no reason to be kept are removed. The only exception
        // is if no IDs are explicitely given and no keep option is set. In this
        // case, the default is to keep the snapshots.
        let default_keep = opts.ids.is_empty() && opts.keep == KeepOptions::default();

        while let Some(sn) = iter.next() {
            let (action, reason) = {
                if sn.must_delete(now) {
                    forget_snaps.push(sn.id);
                    ("remove", "snapshot".to_string())
                } else {
                    match group_keep.matches(sn, last, iter.peek().is_some(), latest_time, now) {
                        None if default_keep => ("keep", "".to_string()),
                        None => {
                            forget_snaps.push(sn.id);
                            ("remove", "".to_string())
                        }
                        Some(reason) => ("keep", reason),
                    }
                }
            };

            let tags = sn.tags.formatln();
            let paths = sn.paths.formatln();
            let time = sn.time.format("%Y-%m-%d %H:%M:%S");
            table.add_row(row![sn.id, time, sn.hostname, tags, paths, action, reason]);

            last = Some(sn);
        }
        table.set_titles(
            row![b->"ID", b->"Time", b->"Host", b->"Tags", b->"Paths", b->"Action", br->"Reason"],
        );
        table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);

        println!();
        table.printstd();
        println!();
    }

    match (forget_snaps.is_empty(), opts.dry_run) {
        (true, _) => println!("nothing to remove"),
        (false, true) => println!(
            "would have removed the following snapshots:\n {:?}",
            forget_snaps
        ),
        (false, false) => {
            println!("removing snapshots...");
            be.delete_list(FileType::Snapshot, forget_snaps.clone(), progress_counter())
                .await?;
        }
    }

    if opts.prune {
        prune::execute(be, opts.prune_opts, config, forget_snaps).await?;
    }

    Ok(())
}

#[derive(Clone, PartialEq, Derivative, Parser)]
#[derivative(Default)]
struct KeepOptions {
    /// keep snapshots with this taglist (can be specified multiple times)
    #[clap(long, value_name = "TAGS")]
    keep_tags: Vec<StringList>,

    /// keep snapshots ids that start with ID (can be specified multiple times)
    #[clap(long = "keep-id", value_name = "ID")]
    keep_ids: Vec<String>,

    /// keep the last N snapshots
    #[clap(long, short = 'l', value_name = "N", default_value = "0")]
    keep_last: u32,

    /// keep the last N hourly snapshots
    #[clap(long, short = 'H', value_name = "N", default_value = "0")]
    keep_hourly: u32,

    /// keep the last N daily snapshots
    #[clap(long, short = 'd', value_name = "N", default_value = "0")]
    keep_daily: u32,

    /// keep the last N weekly snapshots
    #[clap(long, short = 'w', value_name = "N", default_value = "0")]
    keep_weekly: u32,

    /// keep the last N monthly snapshots
    #[clap(long, short = 'm', value_name = "N", default_value = "0")]
    keep_monthly: u32,

    /// keep the last N yearly snapshots
    #[clap(long, short = 'y', value_name = "N", default_value = "0")]
    keep_yearly: u32,

    /// keep snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0h")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within: humantime::Duration,

    /// keep hourly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0h")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within_hourly: humantime::Duration,

    /// keep daily snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0d")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within_daily: humantime::Duration,

    /// keep weekly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0w")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within_weekly: humantime::Duration,

    /// keep monthly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0m")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within_monthly: humantime::Duration,

    /// keep yearly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0y")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    keep_within_yearly: humantime::Duration,
}

fn always_false(_sn1: &SnapshotFile, _sn2: &SnapshotFile) -> bool {
    false
}

fn equal_year(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year()
}

fn equal_month(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.month() == t2.month()
}

fn equal_week(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.iso_week().week() == t2.iso_week().week()
}

fn equal_day(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.ordinal() == t2.ordinal()
}

fn equal_hour(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.ordinal() == t2.ordinal() && t1.hour() == t2.hour()
}

impl KeepOptions {
    fn matches(
        &mut self,
        sn: &SnapshotFile,
        last: Option<&SnapshotFile>,
        has_next: bool,
        latest_time: DateTime<Local>,
        now: DateTime<Local>,
    ) -> Option<String> {
        let mut keep = false;
        let mut reason = String::new();

        if sn.must_keep(now) {
            keep = true;
            reason.push_str("snapshot\n");
        }

        if self
            .keep_ids
            .iter()
            .any(|id| sn.id.to_hex().starts_with(id))
        {
            keep = true;
            reason.push_str("id\n");
        }

        if !self.keep_tags.is_empty() && sn.tags.matches(&self.keep_tags) {
            keep = true;
            reason.push_str("tags\n");
        }

        let keep_checks = [
            (
                always_false as fn(&SnapshotFile, &SnapshotFile) -> bool,
                &mut self.keep_last,
                "last",
                self.keep_within,
                "within",
            ),
            (
                equal_hour,
                &mut self.keep_hourly,
                "hourly",
                self.keep_within_hourly,
                "within hourly",
            ),
            (
                equal_day,
                &mut self.keep_daily,
                "daily",
                self.keep_within_daily,
                "within daily",
            ),
            (
                equal_week,
                &mut self.keep_weekly,
                "weekly",
                self.keep_within_weekly,
                "within weekly",
            ),
            (
                equal_month,
                &mut self.keep_monthly,
                "monthly",
                self.keep_within_monthly,
                "within monthly",
            ),
            (
                equal_year,
                &mut self.keep_yearly,
                "yearly",
                self.keep_within_yearly,
                "within yearly",
            ),
        ];

        for (check_fun, counter, reason1, within, reason2) in keep_checks {
            if !has_next || last.is_none() || !check_fun(sn, last.unwrap()) {
                if *counter > 0 {
                    *counter -= 1;
                    keep = true;
                    reason.push_str(reason1);
                    reason.push('\n');
                }
                if sn.time + Duration::from_std(*within).unwrap() > latest_time {
                    keep = true;
                    reason.push_str(reason2);
                    reason.push('\n');
                }
            }
        }

        keep.then(|| reason)
    }
}
