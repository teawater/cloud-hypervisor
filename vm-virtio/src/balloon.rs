// Copyright (c) 2020 Ant Financial
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::Error as DeviceError;
use super::{
    ActivateError, ActivateResult, DeviceEventT, Queue, VirtioDevice, VirtioDeviceType,
    VIRTIO_F_VERSION_1,
};
use crate::vm_memory::GuestMemory;
use crate::{VirtioInterrupt, VirtioInterruptType};
use epoll;
use libc;
use libc::EFD_NONBLOCK;
use std::cmp;
use std::io::{self, Write};
use std::mem::size_of;
use std::os::unix::io::AsRawFd;
use std::result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use vm_device::{Migratable, MigratableError, Pausable, Snapshotable};
use vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic,
    GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
};
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 128;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES];

// Get resize event.
const RESIZE_EVENT: DeviceEventT = 0;
// New descriptors are pending on the virtio queue.
const INFLATE_QUEUE_AVAIL_EVENT: DeviceEventT = 1;
// New descriptors are pending on the virtio queue.
const DEFLATE_QUEUE_AVAIL_EVENT: DeviceEventT = 2;
// The device has been dropped.
const KILL_EVENT: DeviceEventT = 3;
// The device should be paused.
const PAUSE_EVENT: DeviceEventT = 4;

const PAGE_SHIFT: u32 = 12;

// Size of a PFN in the balloon interface.
const VIRTIO_BALLOON_PFN_SHIFT: u64 = 12;

#[derive(Debug)]
pub enum Error {
    // Guest gave us bad memory addresses.
    GuestMemory(GuestMemoryError),
    // Guest gave us a write only descriptor that protocol says to read from.
    UnexpectedWriteOnlyDescriptor,
    // Guest gave us a read only descriptor that protocol says to write to.
    UnexpectedReadOnlyDescriptor,
    // Guest gave us too few descriptors in a descriptor chain.
    DescriptorChainTooShort,
    // Guest gave us a buffer that was too short to use.
    BufferLengthTooSmall,
    // Guest sent us invalid request.
    InvalidRequest,
    // Failed to EventFd write.
    EventFdWriteFail(std::io::Error),
    // Failed to EventFd try_clone.
    EventFdTryCloneFail(std::io::Error),
    // Failed to MpscRecv.
    MpscRecvFail(mpsc::RecvError),
    // Resize invalid argument
    ResizeInval(String),
    // Fail to resize trigger
    ResizeTriggerFail(DeviceError),
}

// Got from qemu/include/standard-headers/linux/virtio_balloon.h
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default)]
struct VirtioBalloonConfig {
    // Number of pages host wants Guest to give up.
    num_pages: u32,
    // Number of pages we've actually got in balloon.
    actual: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

struct VirtioBalloonResize {
    size: Arc<AtomicU64>,
    tx: mpsc::Sender<Result<(), Error>>,
    rx: Option<mpsc::Receiver<Result<(), Error>>>,
    evt: EventFd,
}

impl VirtioBalloonResize {
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();

        Ok(Self {
            size: Arc::new(AtomicU64::new(0)),
            tx,
            rx: Some(rx),
            evt: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    pub fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            size: self.size.clone(),
            tx: self.tx.clone(),
            rx: None,
            evt: self.evt.try_clone().map_err(Error::EventFdTryCloneFail)?,
        })
    }

    pub fn work(&self, size: u64) -> Result<(), Error> {
        if let Some(rx) = &self.rx {
            self.size.store(size, Ordering::SeqCst);
            self.evt.write(1).map_err(Error::EventFdWriteFail)?;
            rx.recv().map_err(Error::MpscRecvFail)?
        } else {
            panic!("work should not work with cloned resize")
        }
    }

    fn get_size(&self) -> u64 {
        self.size.load(Ordering::SeqCst)
    }

    fn send(&self, r: Result<(), Error>) {
        self.tx.send(r).unwrap();
    }
}

struct BalloonEpollHandler {
    config: Arc<Mutex<VirtioBalloonConfig>>,
    resize: VirtioBalloonResize,
    queues: Vec<Queue>,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    interrupt_cb: Arc<dyn VirtioInterrupt>,
    inflate_queue_evt: EventFd,
    deflate_queue_evt: EventFd,
    kill_evt: EventFd,
    pause_evt: EventFd,
}

impl BalloonEpollHandler {
    fn signal(
        &self,
        int_type: &VirtioInterruptType,
        queue: Option<&Queue>,
    ) -> result::Result<(), DeviceError> {
        self.interrupt_cb.trigger(int_type, queue).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    fn process_queue(&mut self, ev_type: u16) -> result::Result<(), DeviceError> {
        let queue_index = if ev_type == INFLATE_QUEUE_AVAIL_EVENT {
            0
        } else {
            1
        };

        let mut used_desc_heads = [0; QUEUE_SIZE as usize];
        let mut used_count = 0;
        let mem = self.mem.memory();
        for avail_desc in self.queues[queue_index].iter(&mem) {
            // The head contains the request type which MUST be readable.
            if avail_desc.is_write_only() {
                error!("The head contains the request type is not right");
                continue;
            }
            if avail_desc.len as usize % size_of::<u32>() != 0 {
                error!("the request size {} is not right", avail_desc.len);
                continue;
            }

            let mut offset = 0u64;
            while offset < avail_desc.len as u64 {
                let pfn: u32 = match mem.read_obj(GuestAddress(avail_desc.addr.0 + offset)) {
                    Ok(ret) => ret,
                    Err(e) => {
                        error!("Fail to read addr {}: {:?}", avail_desc.addr.0 + offset, e);
                        continue;
                    }
                };
                offset += size_of::<u32>() as u64;

                let pa = pfn << VIRTIO_BALLOON_PFN_SHIFT;
                if let Some(region) = mem.find_region(GuestAddress(pa as u64)) {
                    let addr = region.as_ptr() as u64 + pa as u64 - region.start_addr().raw_value();
                    let advice = if ev_type == INFLATE_QUEUE_AVAIL_EVENT {
                        libc::MADV_DONTNEED
                    } else {
                        libc::MADV_WILLNEED
                    };
                    let res = unsafe {
                        libc::madvise(
                            addr as *mut libc::c_void,
                            (1 << PAGE_SHIFT) as libc::size_t,
                            advice,
                        )
                    };
                    if res != 0 {
                        error!("madvise get error {}", io::Error::last_os_error());
                        continue;
                    }
                } else {
                    error!("Address {} is not available", pa);
                    continue;
                }
            }

            used_desc_heads[used_count] = avail_desc.index;
            used_count += 1;
        }

        for &desc_index in &used_desc_heads[..used_count] {
            self.queues[queue_index].add_used(&mem, desc_index, 0);
        }
        if used_count > 0 {
            self.signal(&VirtioInterruptType::Queue, Some(&self.queues[queue_index]))?;
        }

        Ok(())
    }

    fn run(&mut self, paused: Arc<AtomicBool>) -> result::Result<(), DeviceError> {
        // Create the epoll file descriptor
        let epoll_fd = epoll::create(true).map_err(DeviceError::EpollCreateFd)?;

        // Add events
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.resize.evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(RESIZE_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.inflate_queue_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(INFLATE_QUEUE_AVAIL_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.deflate_queue_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(DEFLATE_QUEUE_AVAIL_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.kill_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(KILL_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.pause_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(PAUSE_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        'epoll: loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(DeviceError::EpollWait(e));
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    RESIZE_EVENT => {
                        if let Err(e) = self.resize.evt.read() {
                            error!("Failed to get resize event: {:?}", e);
                            break 'epoll;
                        } else {
                            let mut need_break = false;
                            let r = {
                                let mut config = self.config.lock().unwrap();
                                config.num_pages = (self.resize.get_size() >> PAGE_SHIFT) as u32;
                                if let Err(e) = self.signal(&VirtioInterruptType::Config, None) {
                                    need_break = true;
                                    Err(Error::ResizeTriggerFail(e))
                                } else {
                                    Ok(())
                                }
                            };
                            if let Err(e) = &r {
                                error!("{:?}", e);
                            }
                            self.resize.send(r);
                            if need_break {
                                break 'epoll;
                            }
                        }
                    }
                    INFLATE_QUEUE_AVAIL_EVENT => {
                        if let Err(e) = self.inflate_queue_evt.read() {
                            error!("Failed to get inflate queue event: {:?}", e);
                            break 'epoll;
                        } else if let Err(e) = self.process_queue(ev_type) {
                            error!("Failed to signal used inflate queue: {:?}", e);
                            break 'epoll;
                        }
                    }
                    DEFLATE_QUEUE_AVAIL_EVENT => {
                        if let Err(e) = self.deflate_queue_evt.read() {
                            error!("Failed to get deflate queue event: {:?}", e);
                            break 'epoll;
                        } else if let Err(e) = self.process_queue(ev_type) {
                            error!("Failed to signal used deflate queue: {:?}", e);
                            break 'epoll;
                        }
                    }
                    KILL_EVENT => {
                        debug!("kill_evt received, stopping epoll loop");
                        break 'epoll;
                    }
                    PAUSE_EVENT => {
                        debug!("PAUSE_EVENT received, pausing virtio-pmem epoll loop");
                        // We loop here to handle spurious park() returns.
                        // Until we have not resumed, the paused boolean will
                        // be true.
                        while paused.load(Ordering::SeqCst) {
                            thread::park();
                        }
                    }
                    _ => {
                        error!("Unknown event for virtio-mem");
                    }
                }
            }
        }

        Ok(())
    }
}

// Virtio device for exposing entropy to the guest OS through virtio.
pub struct Balloon {
    resize: VirtioBalloonResize,
    kill_evt: Option<EventFd>,
    pause_evt: Option<EventFd>,
    avail_features: u64,
    pub acked_features: u64,
    config: Arc<Mutex<VirtioBalloonConfig>>,
    queue_evts: Option<Vec<EventFd>>,
    interrupt_cb: Option<Arc<dyn VirtioInterrupt>>,
    epoll_threads: Option<Vec<thread::JoinHandle<result::Result<(), DeviceError>>>>,
    paused: Arc<AtomicBool>,
}

impl Balloon {
    // Create a new virtio-balloon.
    pub fn new() -> io::Result<Balloon> {
        let avail_features = 1u64 << VIRTIO_F_VERSION_1;

        let config = VirtioBalloonConfig::default();

        Ok(Balloon {
            resize: VirtioBalloonResize::new()?,
            kill_evt: None,
            pause_evt: None,
            avail_features,
            acked_features: 0u64,
            config: Arc::new(Mutex::new(config)),
            queue_evts: None,
            interrupt_cb: None,
            epoll_threads: None,
            paused: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn resize(&self, size: u64) -> Result<(), Error> {
        self.resize.work(size)
    }
}

impl Drop for Balloon {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Balloon {
    fn device_type(&self) -> u32 {
        VirtioDeviceType::TYPE_BALLOON as u32
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        self.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        let mut v = value;
        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature.");

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config = self.config.lock().unwrap();
        let config_slice = config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        warn!("virtio-balloon device configuration is read-only");
    }

    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap>,
        interrupt_cb: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let (self_kill_evt, kill_evt) = EventFd::new(EFD_NONBLOCK)
            .and_then(|e| Ok((e.try_clone()?, e)))
            .map_err(|e| {
                error!("failed creating kill EventFd pair: {}", e);
                ActivateError::BadActivate
            })?;
        self.kill_evt = Some(self_kill_evt);

        let (self_pause_evt, pause_evt) = EventFd::new(EFD_NONBLOCK)
            .and_then(|e| Ok((e.try_clone()?, e)))
            .map_err(|e| {
                error!("failed creating pause EventFd pair: {}", e);
                ActivateError::BadActivate
            })?;
        self.pause_evt = Some(self_pause_evt);

        self.interrupt_cb = Some(interrupt_cb.clone());

        let mut tmp_queue_evts: Vec<EventFd> = Vec::new();
        for queue_evt in queue_evts.iter() {
            // Save the queue EventFD as we need to return it on reset
            // but clone it to pass into the thread.
            tmp_queue_evts.push(queue_evt.try_clone().map_err(|e| {
                error!("failed to clone queue EventFd: {}", e);
                ActivateError::BadActivate
            })?);
        }
        self.queue_evts = Some(tmp_queue_evts);

        let mut handler = BalloonEpollHandler {
            config: self.config.clone(),
            resize: self.resize.try_clone().map_err(|e| {
                error!("failed to clone resize EventFd: {:?}", e);
                ActivateError::BadActivate
            })?,
            queues,
            mem,
            interrupt_cb,
            inflate_queue_evt: queue_evts.remove(0),
            deflate_queue_evt: queue_evts.remove(0),
            kill_evt,
            pause_evt,
        };

        let paused = self.paused.clone();
        let mut epoll_threads = Vec::new();
        thread::Builder::new()
            .name("virtio_balloon".to_string())
            .spawn(move || handler.run(paused))
            .map(|thread| epoll_threads.push(thread))
            .map_err(|e| {
                error!("failed to clone virtio-balloon epoll thread: {}", e);
                ActivateError::BadActivate
            })?;
        self.epoll_threads = Some(epoll_threads);

        Ok(())
    }

    fn reset(&mut self) -> Option<(Arc<dyn VirtioInterrupt>, Vec<EventFd>)> {
        // We first must resume the virtio thread if it was paused.
        if self.pause_evt.take().is_some() {
            self.resume().ok()?;
        }

        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        // Return the interrupt and queue EventFDs
        Some((
            self.interrupt_cb.take().unwrap(),
            self.queue_evts.take().unwrap(),
        ))
    }
}

virtio_pausable!(Balloon);
impl Snapshotable for Balloon {}
impl Migratable for Balloon {}
