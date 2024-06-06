use std::fmt::Write;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Result};
use bytesize::ByteSize;
use futures::{stream::FuturesUnordered, TryStreamExt};
use indicatif::HumanDuration;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use rpassword::{prompt_password, read_password_from_bufread};
use tokio::spawn;
use tokio::time::sleep;
use vlog::*;

use crate::backend::{DecryptReadBackend, FileType, ReadBackend};
use crate::crypto::Key;
use crate::repo::{find_key_in_backend, Id};

const MAX_PASSWORD_RETRIES: usize = 5;

pub fn bytes(b: u64) -> String {
    ByteSize(b).to_string_as(true)
}

pub async fn get_key(
    be: &impl ReadBackend,
    password: Option<&str>,
    password_file: Option<&Path>,
    password_command: Option<&str>,
) -> Result<Key> {
    let password = match (password, password_file, password_command) {
        (Some(pwd), _, _) => Some(pwd.to_string()),
        (_, Some(file), _) => {
            let mut file = BufReader::new(File::open(file)?);
            Some(read_password_from_bufread(&mut file)?)
        }
        (_, _, Some(command)) => {
            let mut commands: Vec<_> = command.split(' ').collect();
            let output = Command::new(commands[0])
                .args(&mut commands[1..])
                .output()?;

            let mut pwd = BufReader::new(&*output.stdout);
            Some(read_password_from_bufread(&mut pwd)?)
        }
        (None, None, None) => None,
    };

    for _ in 0..MAX_PASSWORD_RETRIES {
        match &password {
            // if password is given, directly return the result of find_key_in_backend and don't retry
            Some(pass) => return find_key_in_backend(be, pass, None).await,
            None => {
                // TODO: Differentiate between wrong password and other error!
                if let Ok(key) =
                    find_key_in_backend(be, &prompt_password("enter repository password: ")?, None)
                        .await
                {
                    return Ok(key);
                }
            }
        }
    }
    bail!("incorrect password!");
}

pub fn progress_counter() -> ProgressBar {
    if get_verbosity_level() == 1 {
        let p = ProgressBar::new(0).with_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>10}/{len:10}")
                .unwrap(),
        );
        p.enable_steady_tick(Duration::from_millis(100));
        p
    } else {
        ProgressBar::hidden()
    }
}

pub fn no_progress() -> ProgressBar {
    ProgressBar::hidden()
}

pub fn progress_bytes() -> ProgressBar {
    if get_verbosity_level() == 1 {
        let p = ProgressBar::new(0).with_style(
            ProgressStyle::default_bar()
            .with_key("my_eta", |s: &ProgressState, w: &mut dyn Write| 
                 match (s.pos(), s.len()){
                    (0, _) => write!(w,"-"),
                    (pos,Some(len)) => write!(w,"{:#}", HumanDuration(Duration::from_secs(s.elapsed().as_secs() * (len-pos)/pos))),
                    (_, _) => write!(w,"-"),
                }.unwrap())
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {bytes:>10}/{total_bytes:10} {bytes_per_sec:12} (ETA {my_eta})")
            .unwrap()
            );
        p.enable_steady_tick(Duration::from_secs(1));
        p
    } else {
        ProgressBar::hidden()
    }
}

pub fn warm_up_command(packs: Vec<Id>, command: &str) -> Result<()> {
    for pack in packs {
        let id = pack.to_hex();
        let actual_command = command.replace("%id", &id);
        v1!("calling {actual_command}...");
        let mut commands: Vec<_> = actual_command.split(' ').collect();
        let status = Command::new(commands[0])
            .args(&mut commands[1..])
            .status()?;
        if !status.success() {
            bail!("warm-up command was not successful for pack {id}. {status}");
        }
    }
    Ok(())
}

pub async fn warm_up(be: &impl DecryptReadBackend, packs: Vec<Id>) -> Result<()> {
    let mut be = be.clone();
    be.set_option("retry", "false")?;

    let p = progress_counter();
    p.set_length(packs.len() as u64);
    let mut stream = FuturesUnordered::new();

    const MAX_READER: usize = 20;
    for pack in packs {
        while stream.len() > MAX_READER {
            stream.try_next().await?;
        }

        let p = p.clone();
        let be = be.clone();
        stream.push(spawn(async move {
            // ignore errors as they are expected from the warm-up
            _ = be.read_partial(FileType::Pack, &pack, false, 0, 1).await;
            p.inc(1);
        }))
    }

    stream.try_collect().await?;
    p.finish();

    Ok(())
}

pub async fn wait(d: Option<humantime::Duration>) {
    if let Some(wait) = d {
        v1!("waiting {}...", wait);
        sleep(*wait).await;
    }
}
