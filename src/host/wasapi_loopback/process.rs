//! Process enumeration for per-process audio loopback.
//!
//! Resolves application names to PIDs using the Windows ToolHelp API.
//! Used to drive the `ActivateAudioInterfaceAsync` process loopback mode.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// A resolved process with its PID and executable name.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProcessInfo {
    pub pid: u32,
    pub exe_name: String,
}

/// Find all PIDs whose executable name contains any of the given substrings (case-insensitive).
///
/// This uses `CreateToolhelp32Snapshot` to enumerate all running processes and performs
/// case-insensitive substring matching against each process's executable name.
///
/// Returns an empty `Vec` if no matches are found or if enumeration fails.
pub fn find_pids_by_name_substrings(name_substrings: &[String]) -> Vec<ProcessInfo> {
    if name_substrings.is_empty() {
        return Vec::new();
    }

    let lowercase_substrings: Vec<String> = name_substrings
        .iter()
        .map(|s| s.to_lowercase())
        .collect();

    let mut results = Vec::new();

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return results,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let exe_name = wchar_array_to_string(&entry.szExeFile);
                let exe_lower = exe_name.to_lowercase();

                if lowercase_substrings.iter().any(|sub| exe_lower.contains(sub.as_str())) {
                    results.push(ProcessInfo {
                        pid: entry.th32ProcessID,
                        exe_name,
                    });
                }

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    results
}

/// Find all PIDs whose executable name exactly matches any of the given names (case-insensitive).
#[allow(dead_code)]
pub fn find_pids_by_exact_names(names: &[String]) -> Vec<ProcessInfo> {
    if names.is_empty() {
        return Vec::new();
    }

    let lowercase_names: Vec<String> = names.iter().map(|s| s.to_lowercase()).collect();

    let mut results = Vec::new();

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return results,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let exe_name = wchar_array_to_string(&entry.szExeFile);
                let exe_lower = exe_name.to_lowercase();

                if lowercase_names.iter().any(|name| *name == exe_lower) {
                    results.push(ProcessInfo {
                        pid: entry.th32ProcessID,
                        exe_name,
                    });
                }

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    results
}

/// Convert a null-terminated wide character array to a String.
fn wchar_array_to_string(wchars: &[u16]) -> String {
    let len = wchars.iter().position(|&c| c == 0).unwrap_or(wchars.len());
    let os_string = OsString::from_wide(&wchars[..len]);
    os_string.to_string_lossy().into_owned()
}

/// Deduplicate PIDs by removing those whose ancestor is already in the set.
///
/// Since `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` captures the entire
/// process tree rooted at the target PID, having both a parent and its child in
/// the list would cause the child's audio to be subtracted twice.
///
/// This function walks the parent chain of each PID (via a fresh process snapshot)
/// and removes any PID whose ancestor is also in the set.
pub fn deduplicate_pids_by_tree(pids: &[u32]) -> Vec<u32> {
    if pids.len() <= 1 {
        return pids.to_vec();
    }

    // Deduplicate exact duplicates first
    let mut unique: Vec<u32> = Vec::new();
    for &pid in pids {
        if !unique.contains(&pid) {
            unique.push(pid);
        }
    }

    if unique.len() <= 1 {
        return unique;
    }

    // Build a PID → parent PID map from a process snapshot
    let parent_map = build_parent_map();
    if parent_map.is_empty() {
        return unique;
    }

    // For each PID, walk up its parent chain. If any ancestor is in `unique`, discard this PID.
    let pid_set: std::collections::HashSet<u32> = unique.iter().copied().collect();
    let mut result = Vec::new();

    for &pid in &unique {
        let mut has_ancestor_in_set = false;
        let mut current = pid;
        // Walk up parent chain (limit depth to prevent infinite loops from circular refs)
        for _ in 0..64 {
            match parent_map.get(&current) {
                Some(&parent) if parent != 0 && parent != current => {
                    if pid_set.contains(&parent) {
                        has_ancestor_in_set = true;
                        break;
                    }
                    current = parent;
                }
                _ => break,
            }
        }
        if !has_ancestor_in_set {
            result.push(pid);
        }
    }

    result
}

/// Build a map of PID → parent PID from a process snapshot.
fn build_parent_map() -> std::collections::HashMap<u32, u32> {
    let mut map = std::collections::HashMap::new();

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return map,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    map
}
