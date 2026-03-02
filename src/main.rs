use std::collections::VecDeque;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

struct WorkQueue {
    queue: Mutex<VecDeque<PathBuf>>,
    cv: Condvar,
}

struct SearchConfig {
    target_name: OsString,
    start_dir: PathBuf,
    worker_count: usize,
    sorted_output: bool,
}

fn main() {
    let config = parse_args();

    if !config.start_dir.is_dir() {
        eprintln!(
            "Not a directory: {}",
            config.start_dir.display()
        );
        process::exit(1);
    }

    let queue = Arc::new(WorkQueue {
        queue: Mutex::new(VecDeque::from([config.start_dir.clone()])),
        cv: Condvar::new(),
    });

    let pending_dirs = Arc::new(AtomicUsize::new(1));
    let scanned_entries = Arc::new(AtomicUsize::new(0));
    let io_errors = Arc::new(AtomicUsize::new(0));
    let mut results = Vec::<PathBuf>::new();

    if config
        .start_dir
        .file_name()
        .is_some_and(|name| name == config.target_name.as_os_str())
    {
        results.push(config.start_dir.clone());
    }

    let mut workers = Vec::with_capacity(config.worker_count);
    for _ in 0..config.worker_count {
        let target_name = config.target_name.clone();
        let queue = Arc::clone(&queue);
        let pending_dirs = Arc::clone(&pending_dirs);
        let scanned_entries = Arc::clone(&scanned_entries);
        let io_errors = Arc::clone(&io_errors);

        workers.push(thread::spawn(move || {
            worker_loop(
                &target_name,
                &queue,
                &pending_dirs,
                &scanned_entries,
                &io_errors,
            )
        }));
    }

    for worker in workers {
        match worker.join() {
            Ok(mut local_results) => results.append(&mut local_results),
            Err(_) => {
                eprintln!("One of the worker threads ended with an error");
                process::exit(1);
            }
        }
    }

    if config.sorted_output {
        results.sort_unstable();
    }

    for path in results.iter() {
        println!("{}", path.display());
    }

    eprintln!("Found: {}", results.len());
    eprintln!(
        "Checked: {} | Access errors: {} | Threads: {}",
        scanned_entries.load(Ordering::Relaxed),
        io_errors.load(Ordering::Relaxed),
        config.worker_count
    );
}

fn parse_args() -> SearchConfig {
    let mut args = env::args_os();
    let bin_name = args.next().unwrap_or_else(|| OsString::from("searcher"));
    let mut positional = Vec::with_capacity(2);
    let mut sorted_output = false;

    for arg in args {
        if arg == OsStr::new("--sorted") {
            sorted_output = true;
            continue;
        }

        if arg == OsStr::new("-h") || arg == OsStr::new("--help") {
            print_usage(&bin_name);
            process::exit(0);
        }

        if arg.to_string_lossy().starts_with("--") {
            eprintln!("Unknown flag: {}", arg.to_string_lossy());
            print_usage(&bin_name);
            process::exit(1);
        }

        positional.push(arg);
        if positional.len() > 2 {
            eprintln!("Too many arguments.");
            print_usage(&bin_name);
            process::exit(1);
        }
    }

    let target_name = match positional.first() {
        Some(name) => name.clone(),
        None => {
            print_usage(&bin_name);
            process::exit(1);
        }
    };

    let start_dir = positional
        .get(1)
        .cloned()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let cpu_count = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let worker_count = env::var("SEARCHER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| (cpu_count * 4).clamp(4, 128));

    SearchConfig {
        target_name,
        start_dir,
        worker_count,
        sorted_output,
    }
}

fn print_usage(bin_name: &OsStr) {
    eprintln!(
        "Usage: {} [--sorted] <file name or directory name> [search folder]",
        Path::new(bin_name).display()
    );
}

fn worker_loop(
    target_name: &OsStr,
    queue: &Arc<WorkQueue>,
    pending_dirs: &Arc<AtomicUsize>,
    scanned_entries: &Arc<AtomicUsize>,
    io_errors: &Arc<AtomicUsize>,
) -> Vec<PathBuf> {
    let mut local_results = Vec::new();
    let mut local_scanned_entries = 0usize;
    let mut local_io_errors = 0usize;

    loop {
        let dir = {
            let mut guard = queue.queue.lock().expect("mutex poisoned");
            loop {
                if let Some(path) = guard.pop_front() {
                    break path;
                }

                if pending_dirs.load(Ordering::Acquire) == 0 {
                    scanned_entries.fetch_add(local_scanned_entries, Ordering::Relaxed);
                    io_errors.fetch_add(local_io_errors, Ordering::Relaxed);
                    return local_results;
                }

                guard = queue.cv.wait(guard).expect("mutex poisoned");
            }
        };

        process_directory(
            target_name,
            &dir,
            queue,
            pending_dirs,
            &mut local_results,
            &mut local_scanned_entries,
            &mut local_io_errors,
        );

        if pending_dirs.fetch_sub(1, Ordering::AcqRel) == 1 {
            queue.cv.notify_all();
        }
    }
}

fn process_directory(
    target_name: &OsStr,
    dir: &Path,
    queue: &Arc<WorkQueue>,
    pending_dirs: &Arc<AtomicUsize>,
    local_results: &mut Vec<PathBuf>,
    local_scanned_entries: &mut usize,
    local_io_errors: &mut usize,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            *local_io_errors += 1;
            return;
        }
    };
    let mut discovered_dirs = Vec::new();

    for entry in entries {
        *local_scanned_entries += 1;

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                *local_io_errors += 1;
                continue;
            }
        };

        let path = entry.path();
        let file_name = entry.file_name();
        if file_name == target_name {
            local_results.push(path.clone());
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                *local_io_errors += 1;
                continue;
            }
        };

        if file_type.is_dir() {
            discovered_dirs.push(path);
        }
    }

    if discovered_dirs.is_empty() {
        return;
    }

    pending_dirs.fetch_add(discovered_dirs.len(), Ordering::AcqRel);
    let discovered_count = discovered_dirs.len();

    {
        let mut guard = queue.queue.lock().expect("mutex poisoned");
        guard.extend(discovered_dirs);
    }

    if discovered_count == 1 {
        queue.cv.notify_one();
    } else {
        queue.cv.notify_all();
    }
}
