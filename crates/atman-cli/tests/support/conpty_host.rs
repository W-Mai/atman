use std::{
    collections::BTreeMap,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};

use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, GetHandleInformation,
        HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Diagnostics::ProcessSnapshotting::{
            HPSS, HPSSWALK, PSS_CAPTURE_HANDLES, PSS_HANDLE_ENTRY, PSS_WALK_HANDLES,
            PssCaptureSnapshot, PssFreeSnapshot, PssWalkMarkerCreate, PssWalkMarkerFree,
            PssWalkSnapshot,
        },
        Threading::{GetCurrentProcess, GetProcessId, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    },
};

struct Snapshot(HPSS);

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe { PssFreeSnapshot(GetCurrentProcess(), self.0) };
    }
}

struct Marker(HPSSWALK);

impl Drop for Marker {
    fn drop(&mut self) {
        unsafe { PssWalkMarkerFree(self.0) };
    }
}

pub fn process_handles() -> io::Result<BTreeMap<usize, u32>> {
    let mut snapshot = std::ptr::null_mut();
    let result =
        unsafe { PssCaptureSnapshot(GetCurrentProcess(), PSS_CAPTURE_HANDLES, 0, &mut snapshot) };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let snapshot = Snapshot(snapshot);
    let mut marker = std::ptr::null_mut();
    let result = unsafe { PssWalkMarkerCreate(std::ptr::null(), &mut marker) };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let marker = Marker(marker);
    let mut handles = BTreeMap::new();
    loop {
        let mut entry = PSS_HANDLE_ENTRY::default();
        let result = unsafe {
            PssWalkSnapshot(
                snapshot.0,
                PSS_WALK_HANDLES,
                marker.0,
                std::ptr::from_mut(&mut entry).cast(),
                std::mem::size_of_val(&entry) as u32,
            )
        };
        if result == ERROR_NO_MORE_ITEMS {
            return Ok(handles);
        }
        if result != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let process = unsafe { GetProcessId(entry.Handle) };
        if process != 0 {
            handles.insert(entry.Handle as usize, process);
        }
    }
}

pub struct ConptyHost {
    observed: Result<(u32, OwnedHandle), String>,
}

impl ConptyHost {
    pub fn after_open(before: io::Result<BTreeMap<usize, u32>>) -> Self {
        let observed = (|| {
            let before = before.map_err(|error| format!("before-open handle snapshot: {error}"))?;
            let after = process_handles()
                .map_err(|error| format!("after-open handle snapshot: {error}"))?;
            let mut created = BTreeMap::<u32, Vec<usize>>::new();
            for (handle, process) in after {
                if process != std::process::id() && before.get(&handle) != Some(&process) {
                    created.entry(process).or_default().push(handle);
                }
            }
            if created.len() != 1 {
                return Err(format!(
                    "expected one process created by openpty, observed {created:?}"
                ));
            }
            let (process, handles) = created.pop_first().unwrap();
            let mut last_error = None;
            for handle in handles {
                let mut duplicate = std::ptr::null_mut();
                if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        handle as _,
                        GetCurrentProcess(),
                        &mut duplicate,
                        PROCESS_SYNCHRONIZE,
                        0,
                        0,
                    )
                } == 0
                {
                    last_error = Some(io::Error::last_os_error());
                    continue;
                }
                let observer = unsafe { OwnedHandle::from_raw_handle(duplicate) };
                let mut flags = 0;
                if unsafe { GetHandleInformation(observer.as_raw_handle(), &mut flags) } == 0 {
                    last_error = Some(io::Error::last_os_error());
                    continue;
                }
                if flags & HANDLE_FLAG_INHERIT != 0 {
                    return Err("ConPTY process observer became inheritable".into());
                }
                return Ok((process, observer));
            }
            Err(format!(
                "duplicate original ConPTY process {process}: {last_error:?}"
            ))
        })();
        Self { observed }
    }

    pub fn describe(&self) -> String {
        match &self.observed {
            Ok((process, handle)) => {
                let status = match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
                    WAIT_OBJECT_0 => "exited".to_owned(),
                    WAIT_TIMEOUT => "running".to_owned(),
                    status => format!("wait={status:#x}, error={}", io::Error::last_os_error()),
                };
                format!("Original ConPTY host: pid={process}, process={status}")
            }
            Err(error) => format!("Original ConPTY host: unobserved ({error})"),
        }
    }
}
