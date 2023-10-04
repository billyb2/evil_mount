use anyhow::{anyhow, Context, Result};
use blake3::{Hash, Hasher};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    fs::FileType,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    fs::{self, remove_dir_all, remove_file},
    io,
    task::JoinHandle,
    time::Instant,
};
use walkdir::{DirEntry, WalkDir};

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
                false => match file_type(&path).await.unwrap().is_file() {
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
        for file_info in recursive_dir(&backup_dir) {
            let path = file_info.path();

            let file_type = file_type(&path).await.with_context(|| {
                anyhow!(
                    "Error getting file type of file {} for initialization",
                    file_info.path().display()
                )
            })?;

            if file_type.is_file() || file_type.is_symlink() {
                copy_to_dst(path.to_path_buf(), backup_dir.clone(), work_dir.clone())
                    .await
                    .with_context(|| anyhow!("Error copying file for initialization"))?;
            } else if file_type.is_dir() {
                let work_dir_path = convert_backup_path_to_work_path(
                    path.to_path_buf(),
                    work_dir.clone(),
                    backup_dir.clone(),
                )?;
                fs::create_dir_all(work_dir_path).await?;
            }
        }

        println!("Initialized {}!", work_dir.display());
    }

    let work_dir_clone = work_dir.clone();
    let backup_dir_clone = backup_dir.clone();

    tokio::task::spawn(async move {
        delete_files(work_dir_clone, backup_dir_clone)
            .await
            .unwrap()
    });
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
    /// The tokio task running in a loop that ensures the time is kept in sync
    sync_task: JoinHandle<()>,
}

async fn delete_files(work_dir: PathBuf, backup_dir: PathBuf) -> Result<()> {
    loop {
        for file_info in recursive_dir(&backup_dir).into_iter() {
            // First, check if the path exists in backup_dir
            if !fs::try_exists(&file_info.path()).await.unwrap() {
                continue;
            }
            // If a path exists in backup_dir, but doesn't exist in work_dr, that means the file was deleted in work_dir
            let work_dir_path = convert_backup_path_to_work_path(
                file_info.path().to_path_buf(),
                work_dir.clone(),
                backup_dir.clone(),
            )
            .unwrap();

            if !fs::try_exists(&work_dir_path).await.unwrap() {
                let file_type = file_type(file_info.path())
                    .await
                    .with_context(|| {
                        anyhow!(
                            "Error getting file type for {} for deletion",
                            work_dir_path.display()
                        )
                    })
                    .unwrap();

                if file_type.is_file() || file_type.is_symlink() {
                    fs::remove_file(file_info.path()).await.unwrap();
                } else if file_type.is_dir() {
                    fs::remove_dir_all(file_info.path()).await.unwrap();
                } else {
                    panic!("This is a bug, we're missing some file type")
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// TODO: gitignore
async fn copy_files(work_dir: PathBuf, backup_dir: PathBuf) -> Result<()> {
    println!("Watching for file changes...");

    let mut handles: HashMap<PathBuf, FileSyncInfo> = HashMap::new();

    // Starts any handles that are necessary
    loop {
        for file_info in recursive_dir(&work_dir) {
            if !file_type(file_info.path()).await.unwrap().is_file() {
                continue;
            }

            match handles.get(file_info.path()) {
                Some(FileSyncInfo {
                    sync_task,
                }) => {
                    // Respawn the sync task next loop iteration if it's crashed or finished
                    if sync_task.is_finished() {
                        handles.remove(file_info.path());
                    }
                }
                None => {
                    let backup_path = convert_work_path_to_backup_path(
                        file_info.path().to_path_buf(),
                        work_dir.clone(),
                        backup_dir.clone(),
                    )
                    .unwrap();
                    match fs::metadata(backup_path).await {
                        Ok(metadata) => {
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
                                    sync_task,
                                },
                            );
                        }
                        Err(err) => {
                            match err.kind() {
                                io::ErrorKind::NotFound => {
                                    //TODO: catch this
                                    copy_to_dst(file_info.path().to_path_buf(), work_dir.clone(), backup_dir.clone()).await;
                                },
                                _ => todo!("{err}"),
                            }
                        }
                    }
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

                if current_modify_time != modify_time.load(Ordering::Relaxed) {
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
                if err.kind() == io::ErrorKind::NotFound {
                    return
                } else {
                    todo!("Handle {err} correctly");
                }
            }
        };

        if SHOULD_SHUTDOWN.load(Ordering::Relaxed) {
            return;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn convert_work_path_to_backup_path(
    path: PathBuf,
    work_dir: PathBuf,
    backup_dir: PathBuf,
) -> Result<PathBuf> {
    let new_path = path.strip_prefix(&work_dir).with_context(|| {
        anyhow!(
            "Error stripping prefix {} from {}",
            work_dir.display(),
            path.display()
        )
    })?;
    let mut dst_path = backup_dir.clone();
    dst_path.push(new_path);

    Ok(dst_path)
}
fn convert_backup_path_to_work_path(
    path: PathBuf,
    work_dir: PathBuf,
    backup_dir: PathBuf,
) -> Result<PathBuf> {
    let new_path = path.strip_prefix(&backup_dir).with_context(|| {
        anyhow!(
            "Error stripping prefix {} from {}",
            backup_dir.display(),
            path.display()
        )
    })?;
    let mut dst_path = work_dir.clone();
    dst_path.push(new_path);

    Ok(dst_path)
}

async fn copy_to_dst(path: PathBuf, work_dir: PathBuf, backup_dir: PathBuf) -> Result<()> {
    let dst_path = convert_work_path_to_backup_path(path.clone(), work_dir, backup_dir)?;

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

async fn file_type<P: AsRef<Path>>(path: P) -> Result<FileType> {
    Ok(fs::metadata(path).await?.file_type())
}

pub fn hash_directory(dir: PathBuf) -> Result<HashMap<PathBuf, Hash>> {
    if !dir.exists() {
        return Err(anyhow!(
            "Directory {} does not exist for hashing",
            dir.display()
        ));
    }

    if !dir.is_dir() {
        return Err(anyhow!("Path {} is not a direectory!", dir.display()));
    }

    let file_paths: Vec<_> = recursive_dir(dir.as_ref()).collect();

    // This is just a poor man's try_collect
    let res: Result<Vec<_>> = file_paths
        .into_par_iter()
        .map(|file_info| {
            let mut hasher = Hasher::new();

            let mut file = std::fs::File::open(file_info.path())?;
            std::io::copy(&mut file, &mut hasher)?;

            Ok((file_info.path().to_path_buf(), hasher.finalize()))
        })
        .collect();

    Ok(res?.into_iter().collect())
}

fn recursive_dir(dir: &Path) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(dir)
        .follow_links(false)
        .contents_first(true)
        .into_iter()
        .filter_map(|p| p.ok())
        .filter(|p| p.file_type().is_file())
}
