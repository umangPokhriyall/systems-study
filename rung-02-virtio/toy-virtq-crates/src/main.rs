//! The device half of the same virtqueue, rebuilt on `virtio-queue` and `vm-memory`.
//!
//! It processes the same three requests as `toy-virtq-raw` and must produce the same bytes. As in
//! rung 1, the value is the *diff*: having written the layout by hand first, it is possible to say
//! exactly what these crates do and what they leave to the device model.
//!
//! # What `virtio-queue` provides
//!
//! - **The layout.** Ring addresses, alignment, the `used_event`/`avail_event` placement, and the
//!   16-byte descriptor encoding. All of `layout.rs` in the raw crate, all of it easy to get subtly
//!   wrong, none of it interesting once it works.
//! - **The chain walk, with the bounds.** `DescriptorChain` carries the same `ttl`, the same
//!   `next_index >= queue_size` check and the same 2^32 `yielded_bytes` cap. Getting these right is
//!   the security-critical part, and there is no reason for every VMM to reimplement them.
//! - **Wrapping arithmetic on the free-running counters,** in `Wrapping<u16>` throughout, so a
//!   `<` where a subtraction belongs is hard to write by accident.
//! - **`needs_notification` and `enable_notification`,** including the double-check on re-enable
//!   that closes the sleep-with-work-pending race.
//! - **`MockSplitQueue`,** a driver-side harness that exists precisely so a device model can be
//!   tested without a guest. It is behind the `test-utils` feature.
//!
//! # What it does not provide, and what that costs
//!
//! Everything about *this device*: what the request means, how to gather it, what to write back,
//! and how many bytes were written. That is the entire body of the loop below, and it is also where
//! the bugs that matter live - `used.len` reporting buffer size instead of bytes written, or a
//! response longer than the buffer the driver offered.
//!
//! Two checks the raw crate performs are **not** performed for you:
//!
//! 1. **Buffer bounds.** `vm-memory`'s `read_slice`/`write_slice` check on access, so a bad
//!    descriptor becomes an error rather than a wild access - but only if the device actually goes
//!    through them. A device that took `desc.addr()` and did its own pointer arithmetic would get
//!    no protection at all.
//! 2. **Readable-before-writable ordering.** The spec requires it and `DescriptorChain` does not
//!    enforce it; `readable()`/`writable()` filter the chain but do not object to one that
//!    interleaves them. A device that trusts the ordering can be made to write into a buffer the
//!    driver is still reading.
//!
//! Neither is a defect in the crate - they are device policy - but knowing which side of the line
//! they fall on is exactly what writing the raw version first buys.

use std::error::Error;

use virtio_queue::desc::{RawDescriptor, split::Descriptor as SplitDescriptor};
use virtio_queue::mock::MockSplitQueue;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const REGION: usize = 0x20_0000;
const QUEUE_SIZE: u16 = 8;
/// Where buffers live. `MockSplitQueue` puts the rings low, and its own documentation asks for
/// buffer addresses "at a sufficiently greater location (i.e. 1MiB)".
const ARENA: u64 = 0x10_0000;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

fn main() -> Result<(), Box<dyn Error>> {
    let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), REGION)])?;
    let vq = MockSplitQueue::new(&mem, QUEUE_SIZE);

    // Stand in for the driver. The raw crate has a real one; here `MockSplitQueue` plays that part,
    // which is what it is for - it is the harness `virtio-queue`'s own tests use.
    let requests: [&[&str]; 3] = [&["hello virtio"], &["scatter ", "gather"], &["third"]];

    let mut descs: Vec<RawDescriptor> = Vec::new();
    let mut arena = ARENA;
    let mut reply_addrs = Vec::new();

    for parts in requests {
        for part in parts {
            mem.write_slice(part.as_bytes(), GuestAddress(arena))?;
            let idx = descs.len() as u16;
            descs.push(RawDescriptor::from(SplitDescriptor::new(
                arena,
                part.len() as u32,
                // Never last: the reply descriptor always follows the readable ones.
                VIRTQ_DESC_F_NEXT,
                idx + 1,
            )));
            arena = (arena + part.len() as u64).next_multiple_of(8);
        }
        reply_addrs.push(arena);
        descs.push(RawDescriptor::from(SplitDescriptor::new(
            arena,
            64,
            VIRTQ_DESC_F_WRITE, // last in the chain, so no NEXT
            0,
        )));
        arena += 64;
    }

    vq.add_desc_chains(&descs, 0)?;

    // The device side starts here.
    let mut queue: Queue = vq.create_queue()?;
    queue.set_event_idx(true);

    let mut completions = Vec::new();
    let mut total_descs = 0u64;

    // `iter()` yields one `DescriptorChain` per available chain, and consuming it advances the
    // queue's `next_avail`. This one line replaces `pop_chain_head` plus `ChainIter` in the raw
    // crate - and carries the same three bounds inside it. Collected first because the iterator
    // borrows the queue, and `add_used` needs it mutably.
    for chain in queue.iter(&mem)?.collect::<Vec<_>>() {
        let head = chain.head_index();

        // Gather the device-readable descriptors, then scatter into the writable ones. The chain is
        // cloned because `readable()` and `writable()` each consume it - a small API friction that
        // exists because a chain is an iterator over guest memory, not a collection.
        let mut request = Vec::new();
        for d in chain.clone().readable() {
            total_descs += 1;
            let mut buf = vec![0u8; d.len() as usize];
            mem.read_slice(&mut buf, d.addr())?;
            request.extend_from_slice(&buf);
        }

        let response = request.to_ascii_uppercase();

        let mut written = 0u32;
        let mut cursor = 0usize;
        for d in chain.clone().writable() {
            total_descs += 1;
            if cursor >= response.len() {
                break;
            }
            // min() rather than trusting the response to fit. The device controls `response`; the
            // driver controls `d.len()`. Writing more than the driver offered is a host-controlled
            // write into guest memory at an offset the guest never agreed to.
            let n = (d.len() as usize).min(response.len() - cursor);
            mem.write_slice(&response[cursor..cursor + n], d.addr())?;
            cursor += n;
            written += n as u32;
        }

        // `written`, not the buffer size. See the raw crate's `add_used`.
        queue.add_used(&mem, head, written)?;
        completions.push((head, written));
    }

    // This answer is not trustworthy, and finding out why is what rung 2 actually produced.
    //
    // It disagrees with the hand-written device in `toy-virtq-raw`, which says `true` for the same
    // situation. The cause is a layout bug in `MockSplitQueue`: it places the used ring inside the
    // avail ring, so the avail ring's `used_event` field is overwritten by a used-ring element, and
    // `needs_notification` reads a completion length where a threshold should be.
    //
    // See `tests/mock_layout.rs` for the reproducer and the proposed fix. The bytes above are still
    // correct because this demo publishes only three chains and the corruption starts at avail
    // entry 6 - which is exactly the kind of bug that stays hidden until a queue gets busy.
    let notify = queue.needs_notification(&mem)?;

    println!("== toy-virtq-crates: the same queue on virtio-queue + vm-memory ==");
    println!(
        "  {} chains, {} descriptors, interrupt needed? {}",
        completions.len(),
        total_descs,
        notify
    );

    let mut ok = true;
    for (i, (head, len)) in completions.iter().enumerate() {
        let mut buf = vec![0u8; *len as usize];
        mem.read_slice(&mut buf, GuestAddress(reply_addrs[i]))?;
        let want = requests[i].concat().to_ascii_uppercase();
        let matched = buf == want.as_bytes();
        ok &= matched;
        println!(
            "  used[{i}]: head={head} len={len:<3} {:?}{}",
            String::from_utf8_lossy(&buf),
            if matched { "" } else { "   MISMATCH" }
        );
    }

    if !ok {
        return Err("output does not match toy-virtq-raw".into());
    }
    println!("  output identical to toy-virtq-raw");
    Ok(())
}
