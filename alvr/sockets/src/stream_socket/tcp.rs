use super::{
    MultiplexedSocketReader, MultiplexedSocketWriter, ReconstructedPacket, StreamRecvQueues,
};
use crate::LOCAL_IP;
use alvr_common::{ConResult, HandleTryAgain, ToCon, anyhow::Result, con_bail};
use alvr_session::{DscpTos, SocketBufferConfig};
use socket2::Socket;
use std::{
    collections::HashMap,
    io::Write,
    mem,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

pub const PACKET_PREFIX_SIZE: usize = mem::size_of::<u16>() // stream ID
    + mem::size_of::<u32>() // packet index
    + mem::size_of::<u32>(); // payload size

pub fn bind(
    timeout: Duration,
    port: u16,
    dscp: Option<DscpTos>,
    buffer_config: SocketBufferConfig,
) -> Result<TcpListener> {
    let socket = TcpListener::bind((LOCAL_IP, port))?.into();

    crate::set_socket_buffers(&socket, buffer_config).ok();

    crate::set_dscp(&socket, dscp);

    socket.set_read_timeout(Some(timeout))?;

    Ok(socket.into())
}

pub fn accept_from_server(
    listener: &TcpListener,
    server_ip: Option<IpAddr>,
    timeout: Duration,
) -> ConResult<TcpStream> {
    // Uses timeout set during bind()
    let (socket, server_address) = listener.accept().handle_try_again()?;

    if let Some(ip) = server_ip
        && server_address.ip() != ip
    {
        con_bail!(
            "Connected to wrong client: Expected: {ip}, Found {}",
            server_address.ip()
        );
    }

    socket.set_read_timeout(Some(timeout)).to_con()?;
    socket.set_nodelay(true).to_con()?;

    Ok(socket)
}

pub fn connect_to_client(
    timeout: Duration,
    client_ips: &[IpAddr],
    port: u16,
    buffer_config: SocketBufferConfig,
) -> ConResult<TcpStream> {
    let split_timeout = timeout / client_ips.len() as u32;

    let mut res = alvr_common::try_again();
    for ip in client_ips {
        res = TcpStream::connect_timeout(&SocketAddr::new(*ip, port), split_timeout)
            .handle_try_again();

        if res.is_ok() {
            break;
        }
    }
    let socket = res?.into();

    crate::set_socket_buffers(&socket, buffer_config).ok();
    socket.set_read_timeout(Some(timeout)).to_con()?;

    let socket = TcpStream::from(socket);

    socket.set_nodelay(true).to_con()?;

    Ok(socket)
}

pub struct MultiplexedTcpWriter {
    inner: TcpStream,
}

impl MultiplexedSocketWriter for MultiplexedTcpWriter {
    fn payload_offset(&self) -> usize {
        PACKET_PREFIX_SIZE
    }

    // `buffer` contains the payload offset by `payload_offset()`
    fn send(&mut self, stream_id: u16, packet_index: u32, buffer: &mut Vec<u8>) -> Result<()> {
        let payload_size = buffer.len() - PACKET_PREFIX_SIZE;

        buffer[0..2].copy_from_slice(&stream_id.to_le_bytes());
        buffer[2..6].copy_from_slice(&packet_index.to_le_bytes());
        buffer[6..10].copy_from_slice(&(payload_size as u32).to_le_bytes());

        self.inner.write_all(buffer)?;

        Ok(())
    }
}

struct InProgressPacket {
    stream_id: u16,
    packet_index: u32,
    buffer: Vec<u8>,
    buffer_size: usize,
    cursor: usize,
}

pub struct MultiplexedTcpReader {
    inner: Socket,
    in_progress_packet: Option<InProgressPacket>,
    used_buffers_poll_timeout: Duration,
    // Read-ahead buffer.
    //
    // Unlike UDP, the TCP writer does NOT shard: one video frame is one logical packet of
    // ~350 KB, written with a single write_all (see MultiplexedTcpWriter::send). The cost
    // this buffer removes is on the READ side, where the old code did two syscalls per
    // logical packet - a peek() for the 10-byte prefix (looping until all 10 bytes were
    // there) and then one or more recv() calls to pull the payload in, however many the
    // kernel chose to hand over at once.
    //
    // Pulling the bytes into this buffer with a single large read() per frame instead of
    // 1 + N small ones measurably reduced frame arrival jitter: decoder input interval
    // standard deviation fell 2.84 -> 2.55 ms and the content-to-vsync offset wobble
    // (CHAINEND delta_avg) fell 0.78 -> 0.49 ms, which is what the translational judder
    // was tracking. It matters for wireless exactly as much as for wired; only the link
    // differs, not the syscall pattern.
    read_buf: Vec<u8>,
    read_start: usize,
    read_end: usize,
}

// One read() of this size covers a whole frame at the bitrates in use (a 250 Mbps 90 Hz
// stream is ~350 KB per frame), so the common case is one syscall per frame. Not larger: a
// bigger buffer only adds latency when the stream is idle, and cannot reduce the count below
// one per frame anyway.
const TCP_READ_CHUNK: usize = 512 * 1024;

impl MultiplexedTcpReader {
    /// Make at least `needed` bytes available in read_buf, reading more if necessary.
    fn fill(&mut self, needed: usize) -> ConResult {
        loop {
            let avail = self.read_end - self.read_start;
            if avail >= needed {
                return Ok(());
            }

            // Reclaim the consumed prefix before growing, so the buffer does not creep.
            if self.read_start > 0 {
                self.read_buf.drain(..self.read_start);
                self.read_end -= self.read_start;
                self.read_start = 0;
            }

            let want = TCP_READ_CHUNK.max(needed - avail);
            if self.read_buf.len() < self.read_end + want {
                self.read_buf.resize(self.read_end + want, 0);
            }

            // Read through the io::Read impl: socket2's recv wants MaybeUninit, and this is
            // the same syscall underneath.
            use std::io::Read;
            let n = self
                .inner
                .read(&mut self.read_buf[self.read_end..self.read_end + want])
                .handle_try_again()?;

            // DIAGNOSTIC: how much does one read() actually return?
            //
            // The claim that the kernel hands over a 350 KB frame in several small pieces
            // was an inference from thread wakeup counts, never measured. This records the
            // real distribution so the question is settled by data: if nearly every read
            // returns ~350 KB then one syscall per frame is already the norm and the jitter
            // is elsewhere; if reads return tens of KB the kernel really is dribbling it out.
            if alvr_common::diagnostics_enabled() {
                use std::sync::atomic::{AtomicU64, Ordering};
                static RN: AtomicU64 = AtomicU64::new(0);
                static SUM: AtomicU64 = AtomicU64::new(0);
                static MAX: AtomicU64 = AtomicU64::new(0);
                static HIST: [AtomicU64; 7] = [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ];
                let n64 = n as u64;
                let i = RN.fetch_add(1, Ordering::Relaxed) + 1;
                SUM.fetch_add(n64, Ordering::Relaxed);
                MAX.fetch_max(n64, Ordering::Relaxed);
                let hb = if n64 < 4096 {
                    0
                } else if n64 < 16384 {
                    1
                } else if n64 < 65536 {
                    2
                } else if n64 < 131072 {
                    3
                } else if n64 < 262144 {
                    4
                } else if n64 < 524288 {
                    5
                } else {
                    6
                };
                HIST[hb].fetch_add(1, Ordering::Relaxed);
                if i % 900 == 0 {
                    alvr_common::warn!(
                        "READSIZE n={} avg={:.1}KB max={:.1}KB | hist(<4K,<16K,<64K,<128K,<256K,<512K,512K+)= {}/{}/{}/{}/{}/{}/{}",
                        i,
                        SUM.load(Ordering::Relaxed) as f64 / i as f64 / 1024.0,
                        MAX.load(Ordering::Relaxed) as f64 / 1024.0,
                        HIST[0].load(Ordering::Relaxed),
                        HIST[1].load(Ordering::Relaxed),
                        HIST[2].load(Ordering::Relaxed),
                        HIST[3].load(Ordering::Relaxed),
                        HIST[4].load(Ordering::Relaxed),
                        HIST[5].load(Ordering::Relaxed),
                        HIST[6].load(Ordering::Relaxed),
                    );
                }
            }

            if n == 0 {
                alvr_common::con_bail!("TCP stream closed by peer");
            }

            self.read_end += n;
        }
    }

    fn buffered(&self) -> &[u8] {
        &self.read_buf[self.read_start..self.read_end]
    }
}

impl MultiplexedSocketReader for MultiplexedTcpReader {
    fn payload_offset(&self) -> usize {
        PACKET_PREFIX_SIZE
    }

    fn recv(&mut self, stream_queues: &HashMap<u16, StreamRecvQueues>) -> ConResult {
        if self.in_progress_packet.is_none() {
            // Pull the whole prefix out of the read-ahead buffer in one go.
            self.fill(PACKET_PREFIX_SIZE)?;

            let stream_id;
            let packet_index;
            let payload_size;
            {
                let prefix = &self.buffered()[0..PACKET_PREFIX_SIZE];
                stream_id = u16::from_le_bytes(prefix[0..2].try_into().unwrap());
                packet_index = u32::from_le_bytes(prefix[2..6].try_into().unwrap());
                payload_size = u32::from_le_bytes(prefix[6..10].try_into().unwrap()) as usize;
            }
            self.read_start += PACKET_PREFIX_SIZE;

            let mut buffer = match stream_queues.get(&stream_id) {
                Some(queue) => queue
                    .used_buffer_receiver
                    .recv_timeout(self.used_buffers_poll_timeout)
                    .handle_try_again()?,
                None => {
                    // This is a packet with an invalid stream id, but we must read it anyway. We
                    // can't obtain a used buffer so we create a new one
                    vec![]
                }
            };

            buffer.clear();
            buffer.reserve(PACKET_PREFIX_SIZE + payload_size);

            // The buffer must carry the 10-byte prefix in front of the payload:
            // MultiplexedSocketReader::payload_offset() reports PACKET_PREFIX_SIZE and the
            // consumers index into the buffer with it. Seed it here, then let the payload
            // loop append after it.
            buffer.extend_from_slice(&self.read_buf[self.read_start - PACKET_PREFIX_SIZE..self.read_start]);

            self.in_progress_packet = Some(InProgressPacket {
                stream_id,
                packet_index,
                buffer,
                buffer_size: PACKET_PREFIX_SIZE + payload_size,
                cursor: PACKET_PREFIX_SIZE,
            });
        }

        // Take the in-progress packet out of self so fill() can borrow self mutably.
        // The borrow checker cannot see that the two fields are disjoint through a &mut self
        // method call, so move the value out and put it back.
        let mut ip = self.in_progress_packet.take().unwrap();
        while ip.cursor < ip.buffer_size {
            let remaining = ip.buffer_size - ip.cursor;
            self.fill(remaining)?;

            let take = remaining.min(self.read_end - self.read_start);
            ip.buffer
                .extend_from_slice(&self.read_buf[self.read_start..self.read_start + take]);
            self.read_start += take;
            ip.cursor += take;
        }
        // No set_len here any more: the payload is appended with extend_from_slice, which
        // maintains len() itself. The old code wrote into spare_capacity_mut() and had to
        // publish the length manually; mixing the two would leave the tail uninitialised.

        let stream_id = ip.stream_id;
        let index = ip.packet_index;
        if let Some(queues) = stream_queues.get(&stream_id) {
            queues
                .packet_queue
                .send(ReconstructedPacket {
                    index,
                    buffer: ip.buffer,
                })
                .to_con()?;
        }
        // If the stream ID is unknown the buffer is simply dropped with `ip`.

        Ok(())
    }
}

pub fn split_multiplexed(
    socket: TcpStream,
    used_buffers_poll_timeout: Duration,
) -> Result<(
    Box<dyn MultiplexedSocketWriter + Send>,
    Box<dyn MultiplexedSocketReader + Send>,
)> {
    let writer = MultiplexedTcpWriter {
        inner: socket.try_clone()?,
    };

    let reader = MultiplexedTcpReader {
        inner: socket.into(),
        in_progress_packet: None,
        used_buffers_poll_timeout,
        read_buf: Vec::new(),
        read_start: 0,
        read_end: 0,
    };

    Ok((Box::new(writer), Box::new(reader)))
}
