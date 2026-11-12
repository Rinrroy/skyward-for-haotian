//! What a vbmeta image says the phone may not be brought back below.
//!
//! Android's verified boot refuses to start a slot whose vbmeta image carries
//! a rollback index lower than the one the bootloader keeps in its own store
//! (`avb_slot_verify.c`: `if (vbmeta_header.rollback_index <
//! stored_rollback_index)` ends in `ERROR_ROLLBACK_INDEX`). Writing an old
//! vbmeta image back over a newer system's is therefore exactly how a phone is
//! stopped from starting, so the restore is refused before anything is written.
//!
//! Everything here is pure and total: it reads bytes and never panics, so the
//! rule that guards the write is tested on a computer against hand-built
//! images rather than only on the phone. The offsets are the ones in
//! `external/avb/libavb/avb_vbmeta_image.h`, `avb_descriptor.h`,
//! `avb_chain_partition_descriptor.h` and the walk in `avb_descriptor.c`;
//! `avbtool.py` builds the images these are read from and agrees with them.

/// A vbmeta image's header, the block every image starts with.
const HEADER_LEN: usize = 256;
const MAGIC: &[u8] = b"AVB0";

/// Where the numbers this module is about live in that header. The struct is
/// laid out at these offsets, in this order:
///
/// ```text
///    0 magic[4]
///    4 required_libavb_version_major   u32
///    8 required_libavb_version_minor   u32
///   12 authentication_data_block_size  u64
///   20 auxiliary_data_block_size       u64
///   28 algorithm_type                   u32
///   32 hash_offset                      u64
///   40 hash_size                        u64
///   48 signature_offset                 u64
///   56 signature_size                   u64
///   64 public_key_offset                u64
///   72 public_key_size                  u64
///   80 public_key_metadata_offset       u64
///   88 public_key_metadata_size         u64
///   96 descriptors_offset               u64
///  104 descriptors_size                 u64
///  112 rollback_index                   u64   <- the number the guard is about
///  120 flags                            u32
///  124 rollback_index_location          u32   <- which of the bootloader's slots it is compared in
///  128 release_string[48]
///  176 reserved[80]
/// ```
const AUTH_SIZE_AT: usize = 12;
const AUX_SIZE_AT: usize = 20;
const DESCRIPTORS_OFFSET_AT: usize = 96;
const DESCRIPTORS_SIZE_AT: usize = 104;
const ROLLBACK_AT: usize = 112;
const FLAGS_AT: usize = 120;
const ROLLBACK_LOCATION_AT: usize = 124;

/// A serialized descriptor's own header: `tag` then `num_bytes_following`,
/// both big-endian 64-bit. `num_bytes_following` counts the bytes after this
/// header, which the specification requires to be a multiple of 8.
const DESCRIPTOR_LEN: usize = 16;

/// `AVB_DESCRIPTOR_TAG_CHAIN_PARTITION`, the fourth of the well-known tags:
/// `PROPERTY = 0`, `HASHTREE = 1`, `HASH = 2`, `KERNEL_CMDLINE = 3`,
/// `CHAIN_PARTITION = 4`. A hash descriptor (tag 2) carries no rollback field
/// at all, so a guard keyed on tag 2 would read a digest as a location.
const TAG_CHAIN_PARTITION: u64 = 4;

/// `AvbChainPartitionDescriptor`: the 16-byte descriptor header, then
/// `rollback_index_location` u32, `partition_name_len` u32,
/// `public_key_len` u32, `flags` u32, `reserved[60]`. It names a *location* in
/// the bootloader's store, not an index: the index a chained partition is held
/// to is in that partition's own vbmeta image, which a write here never
/// touches.
const CHAIN_LEN: usize = 92;
const CHAIN_LOCATION_AT: usize = 16;

/// What an image says the phone may not go back past.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rollback {
    /// The image's own index, the header's `rollback_index`. This is the
    /// number `avb_slot_verify` compares against what the bootloader keeps,
    /// and so the number the guard compares.
    pub index: u64,
    /// Where in the bootloader's own store that comparison is made, the
    /// header's `rollback_index_location`. A slot number, not a value; it is
    /// carried so a report can name it, never compared.
    pub location: u32,
    /// The locations the chained partitions this image names are compared in,
    /// ascending. Each is a slot number in the same store. A chained image's
    /// own index is in that image, so writing this one back leaves those
    /// comparisons exactly as they were.
    pub chained: Vec<u32>,
}

impl Rollback {
    /// The number a compare is made on: the image's own index.
    ///
    /// It is deliberately not the highest of anything else. The chained
    /// locations are slots, not indices, and a chained partition's index lives
    /// in that partition's image, which this write does not reach.
    pub fn highest(&self) -> u64 {
        self.index
    }

    /// Whether the image names any chained partition at all, which is what
    /// makes its own index only part of what the phone is held to.
    pub fn chained(&self) -> bool {
        !self.chained.is_empty()
    }
}

/// Reading a header or its descriptors: `Err` says what is wrong with the
/// bytes, and no input makes this panic.
fn u64_at(image: &[u8], at: usize) -> Result<u64, String> {
    let end = at.checked_add(8).ok_or_else(|| format!("a 64-bit word at {at} is past the end of any image"))?;
    let bytes = image.get(at..end).ok_or_else(|| format!("the image is {} bytes, too short for a 64-bit word at {at}", image.len()))?;
    bytes.try_into().map(u64::from_be_bytes).map_err(|_| format!("eight bytes at {at} were not eight bytes"))
}

fn u32_at(image: &[u8], at: usize) -> Result<u32, String> {
    let end = at.checked_add(4).ok_or_else(|| format!("a 32-bit word at {at} is past the end of any image"))?;
    let bytes = image.get(at..end).ok_or_else(|| format!("the image is {} bytes, too short for a 32-bit word at {at}", image.len()))?;
    bytes.try_into().map(u32::from_be_bytes).map_err(|_| format!("four bytes at {at} were not four bytes"))
}

/// Whether `image` starts with a vbmeta header at all, and which flags it
/// carries: the two bits that turn the checks off, as `avbctl` writes them.
pub fn flags(image: &[u8]) -> Result<u32, String> {
    header(image)?;
    u32_at(image, FLAGS_AT)
}

/// The header of `image`, once it is known to be a vbmeta header: `Err` when
/// the bytes are too short or start as something else.
fn header(image: &[u8]) -> Result<(), String> {
    if image.len() < HEADER_LEN {
        return Err(format!("a vbmeta image starts with a {HEADER_LEN}-byte header, and this is {} bytes", image.len()));
    }
    if !image.starts_with(MAGIC) {
        return Err(format!("there is no vbmeta header here: it does not start with {}", String::from_utf8_lossy(MAGIC)));
    }
    Ok(())
}

/// The most a vbmeta image may be. libavb's own limit is 64 KiB
/// (`VBMETA_MAX_SIZE` in `avb_slot_verify.c`), which already allows several
/// chained descriptors and a 4096-bit key.
pub const MOST: usize = 64 * 1024;

/// How long the vbmeta image at the start of `image` is, from its own header:
/// the 256-byte header, then the authentication block, then the auxiliary
/// block. libavb reads an image by exactly these three sizes
/// (`avb_vbmeta_image_verify`), so this is the image and nothing after it.
///
/// A partition is usually larger than the image in it — a `vbmeta` partition
/// is a whole slot's worth of megabytes, and a `boot` partition carries a
/// kernel in front and an AVB footer on its last 64 bytes — so the image's own
/// length is the only thing that says where writing it stops. Writing past it
/// is how a restore would overwrite a footer it was not about.
///
/// `Err` where the header will not read, or where the sizes it names do not
/// add up to something that fits in `image`.
pub fn image_len(image: &[u8]) -> Result<usize, String> {
    header(image)?;
    let auth = u64_at(image, AUTH_SIZE_AT)?;
    let aux = u64_at(image, AUX_SIZE_AT)?;
    let len = (HEADER_LEN as u64)
        .checked_add(auth)
        .and_then(|at| at.checked_add(aux))
        .ok_or_else(|| "the header's block sizes add up to more than any image".to_string())?;
    let len = usize::try_from(len).map_err(|_| "the header's block sizes add up to more than any image".to_string())?;
    if len > MOST {
        return Err(format!("the header says the image is {len} bytes, past the {MOST} a vbmeta image may be"));
    }
    if len > image.len() {
        return Err(format!("the header says the image is {len} bytes, and only {} were read", image.len()));
    }
    Ok(len)
}

/// The rollback index `image` carries, its header's own and the locations its
/// chained partitions name.
pub fn rollback(image: &[u8]) -> Result<Rollback, String> {
    header(image)?;
    let index = u64_at(image, ROLLBACK_AT)?;
    let location = u32_at(image, ROLLBACK_LOCATION_AT)?;
    let chained = chain_locations(image)?;
    Ok(Rollback { index, location, chained })
}

/// The `rollback_index_location` of every chain partition descriptor in
/// `image`, ascending.
///
/// The walk is `avb_descriptor_foreach()`'s. The descriptors are **not** at
/// `descriptors_offset` from the start of the image: that field is an offset
/// into the auxiliary data block, and the auxiliary block begins after the
/// header *and* the authentication block, so the first descriptor is at
/// `256 + authentication_data_block_size + descriptors_offset`. Reading at
/// `descriptors_offset` alone lands inside the signature on a signed image.
fn chain_locations(image: &[u8]) -> Result<Vec<u32>, String> {
    let auth = u64_at(image, AUTH_SIZE_AT)?;
    let offset = u64_at(image, DESCRIPTORS_OFFSET_AT)?;
    let size = u64_at(image, DESCRIPTORS_SIZE_AT)?;
    // The descriptors only have to lie inside the image the header describes;
    // a partition read past its image's end must not let the walk wander into
    // whatever follows.
    let image_len = image_len(image)?;
    let start = (HEADER_LEN as u64)
        .checked_add(auth)
        .and_then(|at| at.checked_add(offset))
        .ok_or_else(|| "the descriptors are said to start past the end of any image".to_string())?;
    let end = start.checked_add(size).ok_or_else(|| "the descriptors are said to run past the end of any image".to_string())?;
    let span = usize::try_from(start).ok().zip(usize::try_from(end).ok()).map(|(start, end)| start..end);
    let Some(span) = span.filter(|span| span.end <= image_len) else {
        return Err(format!(
            "the descriptors are said to be at {start}..{end}, and the image is {image_len} bytes"
        ));
    };
    let mut found = Vec::new();
    let mut at = span.start;
    // Every descriptor takes at least its own 16-byte header, so `at` always
    // moves forward and the walk ends.
    while at < span.end {
        if span.end - at < DESCRIPTOR_LEN {
            return Err(format!("a descriptor at {at} does not fit its own header before the descriptors end at {}", span.end));
        }
        let tag = u64_at(image, at)?;
        let following = u64_at(image, at + 8)?;
        if following % 8 != 0 {
            return Err(format!("the descriptor at {at} says {following} bytes follow it, which is not a multiple of 8"));
        }
        let total = (DESCRIPTOR_LEN as u64)
            .checked_add(following)
            .ok_or_else(|| format!("the descriptor at {at} says it is longer than any image"))?;
        let total = usize::try_from(total).map_err(|_| format!("the descriptor at {at} says it is longer than any image"))?;
        if total > span.end - at {
            return Err(format!("the descriptor at {at} runs past the descriptors' end at {}", span.end));
        }
        // A chain descriptor is read only where its own struct fits, so a
        // truncated one leaves the locations as they were rather than being
        // read out of the next descriptor.
        if tag == TAG_CHAIN_PARTITION && total >= CHAIN_LEN {
            let location = u32_at(image, at + CHAIN_LOCATION_AT)?;
            // The specification refuses a location under 1, so neither is
            // one taken as a location here.
            if location >= 1 {
                found.push(location);
            }
        }
        at += total;
    }
    found.sort_unstable();
    Ok(found)
}

/// Whether a copy may be written back over what the slot holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// The copy's index is not below the phone's: writing it back does not
    /// bring the phone below what it is held to.
    Allowed,
    /// The copy's index is below the phone's. Writing it back is what makes
    /// the bootloader refuse to start the slot, so it is refused here first.
    Refused {
        /// What the slot holds now.
        phone: u64,
        /// What the copy about to be written carries.
        copy: u64,
    },
}

/// The rule that keeps a restore from stopping the phone: the copy may be
/// written back only if its rollback index is **not lower** than the one the
/// slot's image carries now.
///
/// The number the bootloader really compares against lives in its own store,
/// which a recovery cannot read; what the slot's own image carries now is the
/// closest stand-in there is, and it is the one the owner asked for. The
/// comparison is one-sided on purpose: an equal index is allowed (writing the
/// same image back changes nothing the bootloader checks), and anything below
/// it is refused, which is the only direction that is safe to be wrong in.
///
/// `Err` where either image cannot be read as one: a copy whose index cannot
/// be established is not a copy to write back.
pub fn may_restore(phone: &[u8], copy: &[u8]) -> Result<Restore, String> {
    let phone = rollback(phone).map_err(|why| format!("the slot's vbmeta cannot be read: {why}"))?;
    let copy = rollback(copy).map_err(|why| format!("the copy cannot be read: {why}"))?;
    if copy.index < phone.index {
        return Ok(Restore::Refused { phone: phone.index, copy: copy.index });
    }
    Ok(Restore::Allowed)
}

/// A copy of a slot's vbmeta image kept somewhere that outlives a start: what
/// the file is called, the slot it was read from, its size, when it was taken,
/// and the index it carries.
///
/// This is what the page lists and the guard is given. It carries no place,
/// since where the file is kept is the reader's to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    /// The file's own name, which says the slot and when it was taken.
    pub name: String,
    /// The slot the image was read from, empty on a phone with one slot.
    pub suffix: String,
    /// The bytes the file holds.
    pub size: u64,
    /// The index it carries, or why it cannot be read as a vbmeta image: the
    /// page shows either, so a file that cannot be read is never passed over
    /// as though there were nothing there.
    pub rollback: Result<Rollback, String>,
    /// When it was taken, in Unix seconds.
    pub made: i64,
}

impl Copy {
    /// The name the copy made from `suffix`'s image at `made` is kept under.
    ///
    /// A name says the slot and the second it was taken in, and nothing that a
    /// path could be escaped through: it is built here rather than read from a
    /// file, and [`Copy::slot_of`] reads it back.
    pub fn name_for(suffix: &str, made: i64) -> String {
        match suffix {
            "" => format!("vbmeta-{made}.img"),
            suffix => format!("vbmeta{suffix}-{made}.img"),
        }
    }

    /// The slot a copy's own name says it was taken from, empty on a phone
    /// with one slot, where the name is one this recovery wrote.
    pub fn slot_of(name: &str) -> Option<(String, i64)> {
        let rest = name.strip_prefix("vbmeta")?.strip_suffix(".img")?;
        let (slot, made) = match rest.split_once('-') {
            Some((slot, made)) => (slot, made),
            // No suffix and nothing between it and the stamp: a phone with one
            // slot, where the name is `vbmeta-<stamp>` and `split_once` above
            // already took it.
            None => return None,
        };
        if !matches!(slot, "" | "_a" | "_b") {
            return None;
        }
        let made = made.parse().ok()?;
        Some((slot.to_string(), made))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vbmeta image: a header with the given index, flags and descriptor
    /// block, and `auth` bytes of authentication data in between, laid out as
    /// the specification lays it out. The auxiliary block is the descriptor
    /// block, so the image ends where the descriptors do.
    fn image(index: u64, flags: u32, descriptors: &[u8], auth: usize) -> Vec<u8> {
        let mut image = vec![0u8; HEADER_LEN + auth];
        image[..4].copy_from_slice(MAGIC);
        image[12..20].copy_from_slice(&(auth as u64).to_be_bytes());
        image[20..28].copy_from_slice(&((descriptors.len() as u64).div_ceil(64) * 64).to_be_bytes());
        image[96..104].copy_from_slice(&0u64.to_be_bytes());
        image[104..112].copy_from_slice(&(descriptors.len() as u64).to_be_bytes());
        image[112..120].copy_from_slice(&index.to_be_bytes());
        image[120..124].copy_from_slice(&flags.to_be_bytes());
        image[124..128].copy_from_slice(&1u32.to_be_bytes());
        image[128..141].copy_from_slice(b"avbtool 1.3.0");
        image.extend_from_slice(descriptors);
        // The auxiliary block is the descriptors rounded up to 64, which the
        // specification requires of both variable blocks.
        image.resize(HEADER_LEN + auth + (descriptors.len() as u64).div_ceil(64) as usize * 64, 0);
        image
    }

    /// A chain partition descriptor naming `location`, its own struct followed
    /// by `tail` bytes of name and key, rounded up to eight.
    fn chain(location: u32, tail: usize) -> Vec<u8> {
        let following = CHAIN_LEN - DESCRIPTOR_LEN + tail;
        let following = following.div_ceil(8) * 8;
        let mut d = vec![0u8; DESCRIPTOR_LEN + following];
        d[..8].copy_from_slice(&TAG_CHAIN_PARTITION.to_be_bytes());
        d[8..16].copy_from_slice(&(following as u64).to_be_bytes());
        d[16..20].copy_from_slice(&location.to_be_bytes());
        d[20..24].copy_from_slice(&4u32.to_be_bytes());
        d[24..28].copy_from_slice(&(tail as u32).to_be_bytes());
        d
    }

    /// A hash descriptor (tag 2), which carries no rollback field and must
    /// never be read as though it named one.
    fn hash(tail: usize) -> Vec<u8> {
        let following = (72 - DESCRIPTOR_LEN + tail).div_ceil(8) * 8;
        let mut d = vec![0u8; DESCRIPTOR_LEN + following];
        d[..8].copy_from_slice(&2u64.to_be_bytes());
        d[8..16].copy_from_slice(&(following as u64).to_be_bytes());
        // Where a chain descriptor would carry a location, a hash descriptor
        // carries the image size. 5 is a location-looking number on purpose.
        d[16..24].copy_from_slice(&5u64.to_be_bytes());
        d
    }

    #[test]
    fn the_index_is_the_header_s_field_at_112_and_not_the_reserved_padding() {
        let read = rollback(&image(0x0102_0304_0506_0708, 0, &[], 0)).unwrap();
        assert_eq!(read.index, 0x0102_0304_0506_0708);
        assert_eq!(read.highest(), 0x0102_0304_0506_0708);
        // The reserved bytes at 176 must stay out of it: an image that put a
        // value there and zero in the header carries no index at all.
        let mut wrong = image(0, 0, &[], 0);
        wrong[176..184].copy_from_slice(&7u64.to_be_bytes());
        assert_eq!(rollback(&wrong).unwrap().index, 0, "176 is padding, not the index");
    }

    #[test]
    fn the_flags_are_the_header_s_word_at_120() {
        assert_eq!(flags(&image(0, 3, &[], 0)).unwrap(), 3);
        assert_eq!(flags(&image(0, 0x8000_0000, &[], 0)).unwrap(), 0x8000_0000);
    }

    #[test]
    fn descriptors_are_read_from_after_the_authentication_block() {
        // The same descriptor block, once with nothing in front of it and once
        // behind 256 bytes of signature: read at the right place both name the
        // same location.
        let block = chain(3, 8);
        assert_eq!(rollback(&image(1, 0, &block, 0)).unwrap().chained, [3]);
        assert_eq!(rollback(&image(1, 0, &block, 256)).unwrap().chained, [3], "the same block, a signature in front");
        assert_eq!(rollback(&image(1, 0, &block, 512)).unwrap().chained, [3]);

        // The offset rule itself, as a decisive contrast. `image()` puts the
        // block where the specification says it is (256 + auth + offset); a
        // decoy naming location 9 is put where the wrong rule — offset from the
        // start of the image, ignoring the authentication block — would look.
        // The block names 3, so reading 3 is the proof; reading 9 would be the
        // bug.
        let auth = 256;
        let mut contested = image(1, 0, &block, auth);
        let decoy = chain(9, 8);
        contested[HEADER_LEN..HEADER_LEN + decoy.len()].copy_from_slice(&decoy);
        assert_eq!(contested[HEADER_LEN..HEADER_LEN + 8], TAG_CHAIN_PARTITION.to_be_bytes(), "the decoy sits where offset alone points");
        assert_eq!(rollback(&contested).unwrap().chained, [3], "the block at 256 + {auth} + 0 is the one the image means");

        // And a block said to start past the image's own end is an error, not
        // a silent "no chain partitions".
        let mut past = image(1, 0, &block, auth);
        past[96..104].copy_from_slice(&(auth as u64).to_be_bytes());
        assert!(rollback(&past).is_err(), "256 + {auth} + {auth} is past the descriptors");
    }

    #[test]
    fn a_chain_descriptor_is_tag_4_and_its_location_is_at_16() {
        for location in [1u32, 2, 7, u32::MAX] {
            let read = rollback(&image(9, 0, &chain(location, 8), 64)).unwrap();
            assert_eq!(read.chained, [location], "{location}");
            assert_eq!(read.index, 9, "a chain descriptor carries no index of its own");
        }
    }

    #[test]
    fn a_hash_descriptor_is_not_read_as_a_chain_one() {
        // Tag 2, whose bytes 16..24 hold an image size that reads as location
        // 5 if it were mistaken for a chain descriptor. It is not.
        let read = rollback(&image(1, 0, &hash(64), 0)).unwrap();
        assert!(read.chained.is_empty(), "{read:?}");
        assert!(!read.chained());
    }

    #[test]
    fn every_chain_location_is_kept_ascending_and_a_zero_location_is_refused() {
        let mut block = Vec::new();
        block.extend(chain(4, 16));
        block.extend(hash(8));
        block.extend(chain(2, 0));
        block.extend(chain(0, 8));
        let read = rollback(&image(1, 0, &block, 128)).unwrap();
        assert_eq!(read.chained, [2, 4], "ascending, and no location under 1 is one");
        assert!(read.chained());
    }

    #[test]
    fn a_descriptor_walk_that_does_not_add_up_is_an_error_and_never_a_panic() {
        // A size that is not a multiple of 8.
        let mut odd = image(1, 0, &chain(3, 8), 0);
        let at = HEADER_LEN + 8;
        odd[at..at + 8].copy_from_slice(&9u64.to_be_bytes());
        assert!(rollback(&odd).is_err(), "a size of 9 is not a multiple of 8");
        // A descriptor longer than the block it is in.
        let mut over = image(1, 0, &chain(3, 8), 0);
        over[HEADER_LEN + 8..HEADER_LEN + 16].copy_from_slice(&4096u64.to_be_bytes());
        assert!(rollback(&over).is_err(), "a descriptor that runs past the block");
        // A descriptors block said to be outside the image.
        let mut outside = image(1, 0, &chain(3, 8), 0);
        outside[104..112].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(rollback(&outside).is_err());
        let mut far = image(1, 0, &chain(3, 8), 0);
        far[96..104].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(rollback(&far).is_err());
        // A descriptor block that ends with less than a descriptor header left.
        let mut short = image(1, 0, &chain(3, 8), 0);
        short[104..112].copy_from_slice(&(chain(3, 8).len() as u64 + 8).to_be_bytes());
        assert!(rollback(&short).is_err(), "eight bytes left over is not a descriptor header");
    }

    #[test]
    fn what_is_not_a_vbmeta_image_at_all_is_an_error() {
        for not in [&b""[..], &b"AVB0"[..], &vec![0u8; 255][..], &b"ANDROID!"[..]] {
            assert!(rollback(not).is_err(), "{not:?}");
            assert!(flags(not).is_err());
        }
        assert!(rollback(&vec![0u8; HEADER_LEN]).is_err(), "256 zeroes have no magic");
        assert!(rollback(&[b"AVB9".to_vec(), vec![0u8; HEADER_LEN]].concat()).is_err());
    }

    #[test]
    fn the_image_ends_where_the_header_s_three_blocks_end_and_not_at_the_partition_s_end() {
        // A vbmeta partition is a slot's worth of megabytes with a small image
        // at the front: the image's own length is 256 + auth + aux, and the
        // zeroes after it are the partition, not the image.
        let authored = image(1, 0, &chain(3, 8), 256);
        assert_eq!(image_len(&authored).unwrap(), authored.len());
        let mut partition = authored.clone();
        partition.resize(1 << 20, 0);
        assert_eq!(image_len(&partition).unwrap(), authored.len(), "a megabyte partition holds the same image");
        // Which is what stops a write back at the image's end rather than the
        // partition's.
        assert_eq!(&partition[..authored.len()], &authored[..], "the image is the partition's first bytes");

        // A header whose sizes do not add up is an error, not a length.
        let mut short = authored.clone();
        short[20..28].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(image_len(&short).is_err(), "an auxiliary block of u64::MAX");
        let mut past = authored.clone();
        past.truncate(authored.len() - 1);
        assert!(image_len(&past).is_err(), "one byte short of the image it claims");
        // And an image past what a vbmeta image may be is refused outright.
        let mut huge = authored.clone();
        huge.resize(HEADER_LEN + 4096, 0);
        huge[12..20].copy_from_slice(&4096u64.to_be_bytes());
        huge[20..28].copy_from_slice(&(MOST as u64).to_be_bytes());
        assert!(image_len(&huge).is_err(), "past the {MOST} a vbmeta image may be");
    }

    #[test]
    fn a_descriptor_walk_stops_at_the_image_s_own_end_not_the_buffer_s() {
        // The same descriptor block laid past the image the header describes:
        // a partition read to its end must not let the walk find it.
        let block = chain(3, 8);
        let mut image = self::image(1, 0, &[], 128);
        image.extend_from_slice(&block);
        // The header says the auxiliary block is empty, so the image ends at
        // 256 + 128 even though a descriptor sits right after it.
        image[104..112].copy_from_slice(&(block.len() as u64).to_be_bytes());
        assert!(read_chained_only(&image).is_err(), "the descriptors are outside the image the header describes");
        // And it really is past the end the header names.
        let len = image_len(&image).unwrap();
        assert_eq!(len, HEADER_LEN + 128, "the header's own end");
        assert!(len < image.len() - block.len() + 1, "the descriptor block is after it");
    }

    /// The locations a walk finds, ignoring the header's own index, so a test
    /// about the walk does not have to lay out a whole image.
    fn read_chained_only(image: &[u8]) -> Result<Vec<u32>, String> {
        chain_locations(image)
    }

    #[test]
    fn no_length_of_input_from_zero_to_a_few_kilobytes_panics() {
        // Every prefix of a real image, and a fuzz of lengths and fills: the
        // parser has to answer with `Err` rather than stop the recovery.
        let full = {
            let mut block = Vec::new();
            block.extend(chain(2, 24));
            block.extend(hash(16));
            image(5, 2, &block, 512)
        };
        for len in 0..=full.len() {
            let _ = rollback(&full[..len]);
            let _ = flags(&full[..len]);
        }
        for len in [0usize, 1, 7, 8, 15, 16, 95, 96, 103, 104, 111, 112, 119, 120, 123, 127, 175, 255, 256, 257, 1024, 65536] {
            for fill in [0x00u8, 0xff, 0x08, 0x41] {
                let _ = rollback(&vec![fill; len]);
            }
        }
    }

    #[test]
    fn a_copy_below_what_the_phone_holds_is_refused_and_an_equal_or_higher_one_allowed() {
        let phone = image(12, 0, &[], 256);
        assert_eq!(may_restore(&phone, &image(13, 0, &[], 256)), Ok(Restore::Allowed), "a newer copy");
        assert_eq!(may_restore(&phone, &image(12, 0, &[], 256)), Ok(Restore::Allowed), "the same index changes nothing the bootloader checks");
        assert_eq!(may_restore(&phone, &image(11, 0, &[], 256)), Ok(Restore::Refused { phone: 12, copy: 11 }), "an older copy");
        assert_eq!(may_restore(&image(0, 0, &[], 0), &image(0, 0, &[], 0)), Ok(Restore::Allowed), "a phone that holds nothing back");
        assert_eq!(may_restore(&image(1, 0, &[], 0), &image(0, 0, &[], 0)), Ok(Restore::Refused { phone: 1, copy: 0 }));
    }

    #[test]
    fn a_copy_that_cannot_be_read_is_refused_rather_than_written() {
        let phone = image(4, 0, &[], 0);
        for not in [&b""[..], &b"nonsense"[..], &vec![0u8; 300][..]] {
            let refused = may_restore(&phone, not);
            assert!(refused.is_err(), "{not:?} was taken as a copy");
        }
        assert!(may_restore(&vec![0u8; 300], &phone).is_err(), "a slot that cannot be read is not overwritten");
        // A chained partition naming a newer index does not make an older
        // top-level image safe: only the header's own index is compared.
        let chained_old = image(3, 0, &chain(2, 8), 64);
        assert_eq!(may_restore(&image(10, 0, &[], 0), &chained_old), Ok(Restore::Refused { phone: 10, copy: 3 }));
    }

    #[test]
    fn a_copy_is_named_for_its_slot_and_the_second_it_was_taken_and_reads_back() {
        for suffix in ["", "_a", "_b"] {
            let name = Copy::name_for(suffix, 1_792_348_260);
            assert_eq!(Copy::slot_of(&name), Some((suffix.to_string(), 1_792_348_260)), "{name}");
        }
        assert_eq!(Copy::name_for("_a", 7), "vbmeta_a-7.img");
        assert_eq!(Copy::name_for("", 7), "vbmeta-7.img");
        // Names a path could be escaped through are not copies of ours.
        for not in ["../../vbmeta_a-1.img", "vbmeta_c-1.img", "vbmeta_a-1", "vbmeta_a-x.img", "vbmeta_a-1.img.exe", ""] {
            assert_eq!(Copy::slot_of(not), None, "{not:?}");
        }
    }
}
