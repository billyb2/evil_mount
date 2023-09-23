use anyhow::{anyhow, Context, Result};
use blake3::{Hash, Hasher};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering, AtomicBool},
        Arc, Mutex,
    },
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    fs::{self, remove_dir_all, remove_file},
    io,
    task::JoinHandle,
    time::Instant,
};
use walkdir::WalkDir;

use clap::Parser;

/// A program to backup files to a different directory
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The directory that you will be working in, will be completely cleared
    #[arg(short, long)]
    work_dir: PathBuf,

    /// The directory that will be copied to. Used to initialize source dir
    #[arg(short, long)]
    backup_dir: PathBuf,
}

static SHOULD_SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[tokio::main]
async fn main() -> Result<()> {
    let Args {
        work_dir,
        backup_dir,
    } = Args::parse();

    // Ensure that source_dir and backup_dir are folders
    if !work_dir.is_dir() {
        return Err(anyhow!("work_dir must be a directory!"));
    }
    if !backup_dir.is_dir() {
        return Err(anyhow!("backup_dir must be a directory!"));
    }

    println!(
        "Checking if {} and {} are equal",
        work_dir.display(),
        backup_dir.display()
    );

    let work_dir_clone = work_dir.clone();
    let backup_dir_clone = backup_dir.clone();

    let start = Instant::now();

    let (work_dir_hash, backup_dir_hash) = tokio::join!(
        tokio::task::spawn_blocking(move || hash_directory(work_dir_clone)),
        tokio::task::spawn_blocking(move || hash_directory(backup_dir_clone)),
    );

    let work_dir_hash = work_dir_hash??;
    let backup_dir_hash = backup_dir_hash??;

    println!(
        "Done! Took {} seconds",
        Instant::now().duration_since(start).as_secs_f32()
    );

    if work_dir_hash == backup_dir_hash {
        println!(
            "{} == {}, skipping initialization",
            work_dir.display(),
            backup_dir.display()
        );
    } else {
        println!("Clearing {}...", work_dir.display());
        while let Ok(Some(file_info)) = fs::read_dir(&work_dir)
            .await
            .with_context(|| anyhow!("Error reading the source directory"))?
            .next_entry()
            .await
        {
            let path = file_info.path();
            match path.is_dir() {
                true => remove_dir_all(&path).await?,
                false => match path.is_file() {
                    true => remove_file(&path).await?,
                    // not really sure what to do here
                    false => todo!(),
                },
            };
        }
        println!("Cleared {}!", work_dir.display());

        println!(
            "Initializing {} with the contents of {}...",
            work_dir.display(),
            backup_dir.display()
        );
        for file_info in WalkDir::new(&backup_dir)
            .follow_links(true)
            .into_iter()
            .filter(|file_info| match file_info {
                Ok(file_info) => file_info.path().is_file(),
                Err(_) => false,
            })
            .into_iter()
        {
            let file_info = file_info?;
            let path = file_info.path();
            copy_to_dst(path.to_path_buf(), backup_dir.clone(), work_dir.clone())
                .await
                .with_context(|| anyhow!("Error copying file for initialization"))?;
        }

        println!("Initialized {}!", work_dir.display());
    }

    tokio::task::spawn(async move { copy_files(work_dir, backup_dir).await.unwrap() });
    tokio::signal::ctrl_c().await?;

    SHOULD_SHUTDOWN.store(true, Ordering::Relaxed);
    println!("Waiting 5 seconds for tokio tasks to shutdown...");

    tokio::time::sleep(Duration::from_secs(5)).await;

    println!("Done!");

    Ok(())
}

async fn backup_files() {
    todo!()
}

struct FileSyncInfo {
    /// The time the file was last modified to in Unix time
    modify_time: Arc<AtomicU64>,
    /// The tokio task running in a loop that ensures the time is kept in sync
    sync_task: JoinHandle<()>,
}

// TODO: gitignore
async fn copy_files(work_dir: PathBuf, backup_dir: PathBuf) -> Result<()> {
    println!("Watching for file changes...");

    let mut handles: HashMap<PathBuf, FileSyncInfo> = HashMap::new();

    // Starts any handles that are necessary
    loop {
        for file_info in WalkDir::new(&work_dir)
            .follow_links(true)
            .into_iter()
            .filter(|file_info| match file_info {
                Ok(file_info) => file_info.path().is_file(),
                Err(_) => false,
            })
        {
            //FIXME: unwrap
            let file_info = file_info.unwrap();

            match handles.get(file_info.path()) {
                Some(FileSyncInfo {
                    modify_time: _,
                    sync_task,
                }) => {
                    // Respawn the sync task next loop iteration if it's crashed or finished
                    if sync_task.is_finished() {
                        handles.remove(file_info.path());
                    }
                }
                None => {
                    let metadata = fs::metadata(file_info.path()).await.unwrap();
                    let modify_time = Arc::new(AtomicU64::new(
                        metadata
                            .modified()
                            .unwrap()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ));

                    let modify_time_clone = modify_time.clone();
                    let path = file_info.path().to_path_buf();
                    let work_dir = work_dir.clone();
                    let backup_dir = backup_dir.clone();

                    let sync_task = tokio::task::spawn(spawn_sync_task(
                        path,
                        work_dir,
                        backup_dir,
                        modify_time_clone,
                    ));

                    handles.insert(
                        file_info.into_path(),
                        FileSyncInfo {
                            modify_time,
                            sync_task,
                        },
                    );
                }
            }
        }

        if SHOULD_SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// FIXME: return and handle errors
async fn spawn_sync_task(
    path: PathBuf,
    work_dir: PathBuf,
    backup_dir: PathBuf,
    modify_time: Arc<AtomicU64>,
) {
    loop {
        match fs::metadata(path.clone()).await {
            Ok(metadata) => {
                //FIXME: unwrap
                let current_modify_time = metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                if current_modify_time > modify_time.load(Ordering::Relaxed) {
                    modify_time.store(current_modify_time, Ordering::Relaxed);

                    if let Err(err) =
                        copy_to_dst(path.clone(), work_dir.clone(), backup_dir.clone()).await
                    {
                        if let Ok(err) = err.downcast::<io::Error>() {
                            if err.kind() == io::ErrorKind::NotFound {
                                return;
                            } else {
                                Err(err)
                                    .with_context(|| anyhow!("Error syncing file"))
                                    .unwrap()
                            }
                        }
                    }
                }
            }
            Err(err) => {
                match err.kind() {
                    io::ErrorKind::NotFound => {
                        if let Err(err) =
                            copy_to_dst(path.clone(), work_dir.clone(), backup_dir.clone()).await
                        {
                            match err.downcast_ref::<io::Error>() {
                                Some(err) => {
                                    // Ignore file not found errors
                                    if err.kind() != io::ErrorKind::NotFound {
                                        Err(anyhow!(
                                            "Error initializing file in {} due to io::Error: {err}",
                                            backup_dir.display()
                                        ))
                                        .unwrap()
                                    }
                                }
                                None => Err(anyhow!(
                                    "Error initializing file in {}: {err}",
                                    backup_dir.display()
                                ))
                                .unwrap(),
                            }
                        }
                    }
                    _ => todo!(),
                }
            }
        };

        if SHOULD_SHUTDOWN.load(Ordering::Relaxed) {
            return;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn copy_to_dst(path: PathBuf, work_dir: PathBuf, backup_dir: PathBuf) -> Result<()> {
    let new_path = path.strip_prefix(&work_dir).with_context(|| {
        anyhow!(
            "Error stripping prefix {} from {}",
            work_dir.display(),
            path.display()
        )
    })?;
    let mut dst_path = backup_dir.clone();
    dst_path.push(new_path);

    let backup_dir = {
        let mut dst_path = dst_path.clone();
        dst_path.pop();
        dst_path
    };

    fs::create_dir_all(&backup_dir).await?;

    // Becuase of potential write errors when trying to overwrite a write protected file, we simply remove it before copying to it
    if let Err(err) = fs::remove_file(&dst_path).await {
        // We can ignore not found errors, that just means there won't be any conflict
        if err.kind() != io::ErrorKind::NotFound {
            return Err(anyhow!("error removing file {}: {err}", dst_path.display()));
        }
    }

    fs::copy(&path, &dst_path).await.with_context(|| {
        anyhow!(
            "Error copying from {} to {}",
            path.display(),
            dst_path.display()
        )
    })?;

    Ok(())
}

pub fn hash_directory(dir: PathBuf) -> Result<Hash> {
    if !dir.exists() {
        return Err(anyhow!(
            "Directory {} does not exist for hashing",
            dir.display()
        ));
    }

    if !dir.is_dir() {
        return Err(anyhow!("Path {} is not a direectory!", dir.display()));
    }

    let hasher: Arc<Mutex<Hasher>> = Arc::new(Mutex::new(Hasher::new()));

    let mut file_paths: Vec<_> = WalkDir::new(&dir)
        .follow_links(true)
        .into_iter()
        .filter(|file_info| match file_info {
            Ok(file_info) => file_info.path().is_file(),
            Err(_) => false,
        })
        .filter_map(|file_info| file_info.ok())
        .collect();

    file_paths.sort_by(|file_info, file_info2| {
        file_info
            .path()
            .to_string_lossy()
            .to_lowercase()
            .cmp(&file_info2.path().to_string_lossy().to_lowercase())
    });

    for file_info in file_paths.into_iter() {
        let hasher = hasher.clone();

        let mut file = std::fs::File::open(file_info.path())?;
        std::io::copy(&mut file, &mut *hasher.lock().unwrap())?;
    }

    let hasher = &hasher.lock().unwrap();
    Ok(hasher.finalize())
}
