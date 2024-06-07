use std::str::FromStr;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use clap::Parser;
use derivative::Derivative;
use merge::Merge;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};

use super::{progress_counter, prune, table_with_titles, Config};
use crate::backend::{DecryptWriteBackend, FileType};
use crate::repofile::{
    SnapshotFile, SnapshotFilter, SnapshotGroup, SnapshotGroupCriterion, StringList,
};
use crate::repository::OpenRepository;

#[derive(Parser)]
pub(super) struct Opts {
    /// Snapshots to forget. If none is given, use filter options to filter from all snapshots
    #[clap(value_name = "ID")]
    ids: Vec<String>,

    #[clap(flatten)]
    config: ConfigOpts,

    #[clap(
        flatten,
        next_help_heading = "PRUNE OPTIONS (only when used with --prune)"
    )]
    prune_opts: prune::Opts,
}

#[serde_as]
#[derive(Clone, Default, Debug, Parser, Deserialize, Merge)]
#[serde(default, rename_all = "kebab-case")]
pub struct ConfigOpts {
    /// Group snapshots by any combination of host,label,paths,tags (default: "host,label,paths")
    #[clap(long, short = 'g', value_name = "CRITERION")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    group_by: Option<SnapshotGroupCriterion>,

    /// Also prune the repository
    #[clap(long)]
    #[merge(strategy = merge::bool::overwrite_false)]
    prune: bool,

    #[clap(flatten, next_help_heading = "Snapshot filter options")]
    #[serde(flatten)]
    filter: SnapshotFilter,

    #[clap(flatten, next_help_heading = "Retention options")]
    #[serde(flatten)]
    keep: KeepOptions,
}

pub(super) fn execute(repo: OpenRepository, config: Config, mut opts: Opts) -> Result<()> {
    let be = &repo.dbe;
    // merge "forget" section from config file, if given
    opts.config.merge(config.forget.clone());
    // merge "snapshot-filter" section from config file, if given
    opts.config.filter.merge(config.snapshot_filter.clone());

    let group_by = opts
        .config
        .group_by
        .unwrap_or_else(|| SnapshotGroupCriterion::from_str("host,label,paths").unwrap());

    let groups = match opts.ids.is_empty() {
        true => SnapshotFile::group_from_backend(be, &opts.config.filter, &group_by)?,
        false => vec![(
            SnapshotGroup::default(),
            SnapshotFile::from_ids(be, &opts.ids)?,
        )],
    };
    let mut forget_snaps = Vec::new();

    for (group, mut snapshots) in groups {
        if !group.is_empty() {
            println!("snapshots for {group}");
        }
        snapshots.sort_unstable_by(|sn1, sn2| sn1.cmp(sn2).reverse());
        let latest_time = snapshots[0].time;
        let mut group_keep = opts.config.keep.clone();
        let mut table = table_with_titles([
            "ID", "Time", "Host", "Label", "Tags", "Paths", "Action", "Reason",
        ]);

        let mut iter = snapshots.iter().peekable();
        let mut last = None;
        let now = Local::now();
        // snapshots that have no reason to be kept are removed. The only exception
        // is if no IDs are explicitly given and no keep option is set. In this
        // case, the default is to keep the snapshots.
        let default_keep = opts.ids.is_empty() && group_keep == KeepOptions::default();

        while let Some(sn) = iter.next() {
            let (action, reason) = {
                if sn.must_keep(now) {
                    ("keep", "snapshot".to_string())
                } else if sn.must_delete(now) {
                    forget_snaps.push(sn.id);
                    ("remove", "snapshot".to_string())
                } else if !opts.ids.is_empty() {
                    forget_snaps.push(sn.id);
                    ("remove", "id argument".to_string())
                } else {
                    match group_keep.matches(sn, last, iter.peek().is_some(), latest_time) {
                        None if default_keep => ("keep", String::new()),
                        None => {
                            forget_snaps.push(sn.id);
                            ("remove", String::new())
                        }
                        Some(reason) => ("keep", reason),
                    }
                }
            };

            let tags = sn.tags.formatln();
            let paths = sn.paths.formatln();
            let time = sn.time.format("%Y-%m-%d %H:%M:%S").to_string();
            table.add_row([
                &sn.id.to_string(),
                &time,
                &sn.hostname,
                &sn.label,
                &tags,
                &paths,
                action,
                &reason,
            ]);

            last = Some(sn);
        }

        println!();
        println!("{table}");
        println!();
    }

    match (forget_snaps.is_empty(), config.global.dry_run) {
        (true, _) => println!("nothing to remove"),
        (false, true) => println!("would have removed the following snapshots:\n {forget_snaps:?}"),
        (false, false) => {
            let p = progress_counter("removing snapshots...");
            be.delete_list(FileType::Snapshot, true, forget_snaps.iter(), p)?;
        }
    }

    if opts.config.prune {
        prune::execute(repo, config, opts.prune_opts, forget_snaps)?;
    }

    Ok(())
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Derivative, Parser, Deserialize, Merge)]
#[derivative(Default)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub(super) struct KeepOptions {
    /// Keep snapshots with this taglist (can be specified multiple times)
    #[clap(long, value_name = "TAG[,TAG,..]")]
    #[serde_as(as = "Vec<DisplayFromStr>")]
    #[merge(strategy=merge::vec::overwrite_empty)]
    keep_tags: Vec<StringList>,

    /// Keep snapshots ids that start with ID (can be specified multiple times)
    #[clap(long = "keep-id", value_name = "ID")]
    #[merge(strategy=merge::vec::overwrite_empty)]
    keep_ids: Vec<String>,

    /// Keep the last N snapshots (N == -1: keep all snapshots)
    #[clap(long, short = 'l', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_last: i32,

    /// Keep the last N hourly snapshots (N == -1: keep all hourly snapshots)
    #[clap(long, short = 'H', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_hourly: i32,

    /// Keep the last N daily snapshots (N == -1: keep all daily snapshots)
    #[clap(long, short = 'd', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_daily: i32,

    /// Keep the last N weekly snapshots (N == -1: keep all weekly snapshots)
    #[clap(long, short = 'w', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_weekly: i32,

    /// Keep the last N monthly snapshots (N == -1: keep all monthly snapshots)
    #[clap(long, short = 'm', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_monthly: i32,

    /// Keep the last N quarter-yearly snapshots (N == -1: keep all quarter-yearly snapshots)
    #[clap(long, value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_quarter_yearly: i32,

    /// Keep the last N half-yearly snapshots (N == -1: keep all half-yearly snapshots)
    #[clap(long, value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_half_yearly: i32,

    /// Keep the last N yearly snapshots (N == -1: keep all yearly snapshots)
    #[clap(long, short = 'y', value_name = "N", default_value = "0", allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    #[merge(strategy=merge::num::overwrite_zero)]
    keep_yearly: i32,

    /// Keep snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0h")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within: humantime::Duration,

    /// Keep hourly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0h")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_hourly: humantime::Duration,

    /// Keep daily snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0d")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_daily: humantime::Duration,

    /// Keep weekly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0w")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_weekly: humantime::Duration,

    /// Keep monthly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0m")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_monthly: humantime::Duration,

    /// Keep quarter-yearly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0y")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_quarter_yearly: humantime::Duration,

    /// Keep half-yearly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0y")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_half_yearly: humantime::Duration,

    /// Keep yearly snapshots newer than DURATION relative to latest snapshot
    #[clap(long, value_name = "DURATION", default_value = "0y")]
    #[derivative(Default(value = "std::time::Duration::ZERO.into()"))]
    #[serde_as(as = "DisplayFromStr")]
    #[merge(strategy=overwrite_zero_duration)]
    keep_within_yearly: humantime::Duration,
}

fn overwrite_zero_duration(left: &mut humantime::Duration, right: humantime::Duration) {
    if *left == std::time::Duration::ZERO.into() {
        *left = right;
    }
}

fn always_false(_sn1: &SnapshotFile, _sn2: &SnapshotFile) -> bool {
    false
}

fn equal_year(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year()
}

fn equal_half_year(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.month0() / 6 == t2.month0() / 6
}

fn equal_quarter_year(sn1: &SnapshotFile, sn2: &SnapshotFile) -> bool {
    let (t1, t2) = (sn1.time, sn2.time);
    t1.year() == t2.year() && t1.month0() / 3 == t2.month0() / 3
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
    ) -> Option<String> {
        let mut keep = false;
        let mut reason = Vec::new();

        let snapshot_id_hex = sn.id.to_hex();
        if self
            .keep_ids
            .iter()
            .any(|id| snapshot_id_hex.starts_with(id))
        {
            keep = true;
            reason.push("id");
        }

        if !self.keep_tags.is_empty() && sn.tags.matches(&self.keep_tags) {
            keep = true;
            reason.push("tags");
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
                equal_quarter_year,
                &mut self.keep_quarter_yearly,
                "quarter-yearly",
                self.keep_within_quarter_yearly,
                "within quarter-yearly",
            ),
            (
                equal_half_year,
                &mut self.keep_half_yearly,
                "half-yearly",
                self.keep_within_half_yearly,
                "within half-yearly",
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
                if *counter != 0 {
                    keep = true;
                    reason.push(reason1);
                    if *counter > 0 {
                        *counter -= 1;
                    }
                }
                if sn.time + Duration::from_std(*within).unwrap() > latest_time {
                    keep = true;
                    reason.push(reason2);
                }
            }
        }

        keep.then_some(reason.join("\n"))
    }
}
