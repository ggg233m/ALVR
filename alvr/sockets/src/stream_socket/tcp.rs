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
    // ~300 KB, written with a single write_all (see MultiplexedTcpWriter::send).
    //
    // What this buffer actually buys, measured by the READSIZE probe below (11 sessions,
    // 198-235 Mbps): on the client video stream the kernel returns 51-61 KB per read() no
    // matter how much is asked for, so a ~300 KB frame still arrives in ~5 reads. A 512 KB
    // request does NOT make it one read per frame. The delivered chunk is set by Windows
    // Receive Window Auto-Tuning: raising the requested receive buffer from the 64 KB
    // default to i32::MAX left the average chunk at 60.3 KB (from 60.6 KB), i.e. unchanged.
    // (The same probe on the server's control socket reports 1.4-2.3 KB, which is correct
    // for small control packets and says nothing about the video path.)
    //
    // So the saving is 1 syscall, not N. The old path did recv(MSG_PEEK) into a 10-byte
    // buffer to learn the prefix, then the same ~5 recv() calls for the payload: ~6 per
    // frame -> ~5. The peek itself was not a spin (the socket has a read timeout, so it
    // blocks; and because ~60 KB is already queued it normally returns all 10 bytes on the
    // first call) - it was one extra syscall, and a second read of the same 10 bytes.
    //
    // Do not attribute a jitter improvement to this. An earlier version of this comment
    // claimed "one read() per frame" and cited a standard-deviation drop of 2.84 -> 2.55 ms
    // plus a CHAINEND delta_avg drop of 0.78 -> 0.49 ms. Neither holds up: no code on the
    // client computes a standard deviation at all (the 2.840/2.555 pair is PRESENTINT
    // jitter_avg, a server-side mean absolute difference of consecutive Present intervals,
    // from one disturbed session running 78.8 fps with a 1705 ms outlier), and 0.49 ms
    // delta_avg appears nowhere in any capture - the lowest non-warmup value ever recorded
    // is 1.34 ms. There is also no pre-read-ahead session log to compare against: READSIZE
    // was added in the same commit as this buffer, so every log carrying CHAINEND also
    // carries the new reader.
    read_buf: Vec<u8>,
    read_start: usize,
    read_end: usize,
}

// Requested size per read(). This is an upper bound, not what the kernel hands back: measured
// delivery is 51-61 KB regardless (see the note on MultiplexedTcpReader::read_buf). There is
// no point raising it - the delivered chunk is set by automatic receive-window tuning, and
// the buffer only costs a memcpy of whatever it did return.
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
            // This was added to settle an inference: that the kernel hands a ~350 KB frame
            // over in several small pieces. Answer, from 11 sessions at 198-235 Mbps:
            // average 51-61 KB per read, ~5 reads per frame, peak 512 KB reached only in a
            // handful of samples. The inference was right, and the 512 KB request does not
            // change it. Kept because it is the only direct measurement of this.
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
                static SEEN_EPOCH: AtomicU64 = AtomicU64::new(0);
                if alvr_common::new_probe_session(&SEEN_EPOCH) {
                    RN.store(0, Ordering::Relaxed);
                    SUM.store(0, Ordering::Relaxed);
                    MAX.store(0, Ordering::Relaxed);
                    for slot in HIST.iter() {
                        slot.store(0, Ordering::Relaxed);
                    }
                }
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
