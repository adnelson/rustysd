//! collect the different streams from the services
//! Stdout and stderr get redirected to the normal stdout/err but are prefixed with a unique string to identify their output
//! streams from the notification sockets get parsed and applied to the respective service

use crate::platform::reset_event_fd;
use crate::platform::EventFd;
use crate::services::Service;
use crate::units::*;
use std::{collections::HashMap, os::unix::io::AsRawFd};

fn collect_from_srvc<F>(unit_table: ArcMutUnitTable, f: F) -> HashMap<i32, UnitId>
where
    F: Fn(&mut HashMap<i32, UnitId>, &Service, UnitId),
{
    unit_table
        .read()
        .unwrap()
        .iter()
        .fold(HashMap::new(), |mut map, (id, srvc_unit)| {
            let srvc_unit_locked = srvc_unit.lock().unwrap();
            if let UnitSpecialized::Service(srvc) = &srvc_unit_locked.specialized {
                f(&mut map, &srvc, id.clone());
            }
            map
        })
}

pub fn handle_all_streams(eventfd: EventFd, unit_table: ArcMutUnitTable) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id = collect_from_srvc(unit_table.clone(), |map, srvc, id| {
            if let Some(socket) = &srvc.notifications {
                map.insert(socket.as_raw_fd(), id);
            }
        });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted notification select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let srvc_unit_locked = &mut *srvc_unit.lock().unwrap();
                            if let UnitSpecialized::Service(srvc) =
                                &mut srvc_unit_locked.specialized
                            {
                                if let Some(socket) = &srvc.notifications {
                                    let old_flags =
                                        nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_GETFL)
                                            .unwrap();

                                    let old_flags =
                                        nix::fcntl::OFlag::from_bits(old_flags).unwrap();
                                    let mut new_flags = old_flags.clone();
                                    new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
                                    nix::fcntl::fcntl(
                                        *fd,
                                        nix::fcntl::FcntlArg::F_SETFL(new_flags),
                                    )
                                    .unwrap();
                                    let bytes = {
                                        match socket.recv(&mut buf[..]) {
                                            Ok(b) => b,
                                            Err(e) => match e.kind() {
                                                std::io::ErrorKind::WouldBlock => 0,
                                                _ => panic!("{}", e),
                                            },
                                        }
                                    };
                                    nix::fcntl::fcntl(
                                        *fd,
                                        nix::fcntl::FcntlArg::F_SETFL(old_flags),
                                    )
                                    .unwrap();
                                    let note_str =
                                        String::from_utf8(buf[..bytes].to_vec()).unwrap();
                                    srvc.notifications_buffer.push_str(&note_str);
                                    crate::notification_handler::handle_notifications_from_buffer(
                                        srvc,
                                        &srvc_unit_locked.conf.name(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_all_std_out(eventfd: EventFd, run_info: ArcRuntimeInfo) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id = collect_from_srvc(run_info.unit_table.clone(), |map, srvc, id| {
            if let Some(fd) = &srvc.stdout_dup {
                map.insert(fd.0, id);
            }
        });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted stdout select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*run_info.unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let mut srvc_unit_locked = srvc_unit.lock().unwrap();
                            let name = srvc_unit_locked.conf.name();
                            let status_table_locked = run_info.status_table.read().unwrap();
                            let status = status_table_locked
                                .get(&srvc_unit_locked.id)
                                .unwrap()
                                .lock()
                                .unwrap();

                            let old_flags =
                                nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
                            let old_flags = nix::fcntl::OFlag::from_bits(old_flags).unwrap();
                            let mut new_flags = old_flags.clone();
                            new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
                            nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_SETFL(new_flags))
                                .unwrap();

                            ////
                            let bytes = match nix::unistd::read(*fd, &mut buf[..]) {
                                Ok(b) => b,
                                Err(nix::Error::Sys(nix::errno::EWOULDBLOCK)) => 0,
                                Err(e) => panic!("{}", e),
                            };
                            ////

                            nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_SETFL(old_flags))
                                .unwrap();

                            if let UnitSpecialized::Service(srvc) =
                                &mut srvc_unit_locked.specialized
                            {
                                srvc.stdout_buffer.extend(&buf[..bytes]);
                                srvc.log_stdout_lines(&name, &status).unwrap();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_all_std_err(eventfd: EventFd, run_info: ArcRuntimeInfo) {
    loop {
        // need to collect all again. There might be a newly started service
        let fd_to_srvc_id = collect_from_srvc(run_info.unit_table.clone(), |map, srvc, id| {
            if let Some(fd) = &srvc.stderr_dup {
                map.insert(fd.0, id);
            }
        });

        let mut fdset = nix::sys::select::FdSet::new();
        for fd in fd_to_srvc_id.keys() {
            fdset.insert(*fd);
        }
        fdset.insert(eventfd.read_end());

        let result = nix::sys::select::select(None, Some(&mut fdset), None, None, None);
        match result {
            Ok(_) => {
                if fdset.contains(eventfd.read_end()) {
                    trace!("Interrupted stderr select because the eventfd fired");
                    reset_event_fd(eventfd);
                    trace!("Reset eventfd value");
                }
                let mut buf = [0u8; 512];
                let unit_table_locked = &*run_info.unit_table.read().unwrap();
                for (fd, id) in &fd_to_srvc_id {
                    if fdset.contains(*fd) {
                        if let Some(srvc_unit) = unit_table_locked.get(id) {
                            let mut srvc_unit_locked = srvc_unit.lock().unwrap();
                            let name = srvc_unit_locked.conf.name();
                            let status_table_locked = run_info.status_table.read().unwrap();
                            let status = status_table_locked
                                .get(&srvc_unit_locked.id)
                                .unwrap()
                                .lock()
                                .unwrap();

                            let old_flags =
                                nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
                            let old_flags = nix::fcntl::OFlag::from_bits(old_flags).unwrap();
                            let mut new_flags = old_flags.clone();
                            new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
                            nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_SETFL(new_flags))
                                .unwrap();

                            ////
                            let bytes = match nix::unistd::read(*fd, &mut buf[..]) {
                                Ok(b) => b,
                                Err(nix::Error::Sys(nix::errno::EWOULDBLOCK)) => 0,
                                Err(e) => panic!("{}", e),
                            };
                            ////
                            nix::fcntl::fcntl(*fd, nix::fcntl::FcntlArg::F_SETFL(old_flags))
                                .unwrap();

                            if let UnitSpecialized::Service(srvc) =
                                &mut srvc_unit_locked.specialized
                            {
                                srvc.stderr_buffer.extend(&buf[..bytes]);
                                srvc.log_stderr_lines(&name, &status).unwrap();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error while selecting: {}", e);
            }
        }
    }
}

pub fn handle_notification_message(msg: &str, srvc: &mut Service, name: &str) {
    let split: Vec<_> = msg.split('=').collect();
    match split[0] {
        "STATUS" => {
            srvc.status_msgs.push(split[1].to_owned());
            trace!(
                "New status message pushed from service {}: {}",
                name,
                srvc.status_msgs.last().unwrap()
            );
        }
        "READY" => {
            srvc.signaled_ready = true;
        }
        _ => {
            warn!("Unknown notification name{}", split[0]);
        }
    }
}

pub fn handle_notifications_from_buffer(srvc: &mut Service, name: &str) {
    while srvc.notifications_buffer.contains('\n') {
        let (line, rest) = srvc
            .notifications_buffer
            .split_at(srvc.notifications_buffer.find('\n').unwrap());
        let line = line.to_owned();
        srvc.notifications_buffer = rest[1..].to_owned();

        handle_notification_message(&line, srvc, name);
    }
}
