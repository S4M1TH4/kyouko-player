//! Build-time ICO encoder. Uses standard 32-bit DIB frames with alpha and
//! transparency masks; all resolutions share the mountain-and-echo geometry.
#[path = "../src/ui/glyph.rs"]
mod glyph;

const SIZES: [usize; 4] = [16, 32, 48, 256];
const RGB: [u8; 3] = [0xA8, 0xE0, 0xBE];
const SUPERSAMPLE: usize = 4;

pub fn encode() -> Vec<u8> {
    let mut bytes = vec![0, 0, 1, 0, SIZES.len() as u8, 0];
    bytes.resize(6 + 16 * SIZES.len(), 0);
    for (entry, size) in SIZES.into_iter().enumerate() {
        let frame = dib(size);
        let offset = bytes.len() as u32;
        let directory = &mut bytes[6 + entry * 16..6 + (entry + 1) * 16];
        // ICO encodes the 256-pixel dimension as zero.
        directory[0] = size as u8;
        directory[1] = size as u8;
        directory[4..6].copy_from_slice(&1u16.to_le_bytes());
        directory[6..8].copy_from_slice(&32u16.to_le_bytes());
        directory[8..12].copy_from_slice(&(frame.len() as u32).to_le_bytes());
        directory[12..16].copy_from_slice(&offset.to_le_bytes());
        bytes.extend(frame);
    }
    bytes
}

fn dib(size: usize) -> Vec<u8> {
    let mask = glyph::glyph_mask_at(size * SUPERSAMPLE);
    let mask_stride = size.div_ceil(32) * 4;
    let xor_size = size * size * 4;
    let mut frame = vec![0; 40 + xor_size + mask_stride * size];
    frame[0..4].copy_from_slice(&40u32.to_le_bytes());
    frame[4..8].copy_from_slice(&(size as i32).to_le_bytes());
    // DIB height includes both the color bitmap and the 1-bit AND mask.
    frame[8..12].copy_from_slice(&((size * 2) as i32).to_le_bytes());
    frame[12..14].copy_from_slice(&1u16.to_le_bytes());
    frame[14..16].copy_from_slice(&32u16.to_le_bytes());
    frame[20..24].copy_from_slice(&(xor_size as u32).to_le_bytes());
    for y in 0..size {
        for x in 0..size {
            let mut coverage = 0;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    coverage += usize::from(mask[y * SUPERSAMPLE + sy][x * SUPERSAMPLE + sx]);
                }
            }
            let alpha = (coverage * 255 / SUPERSAMPLE.pow(2)) as u8;
            let bottom_up_y = size - 1 - y;
            let pixel = 40 + (bottom_up_y * size + x) * 4;
            // ICO stores straight alpha.
            frame[pixel..pixel + 4].copy_from_slice(&[RGB[2], RGB[1], RGB[0], alpha]);
            if alpha == 0 {
                frame[40 + xor_size + bottom_up_y * mask_stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn ico_contains_all_sizes_and_valid_color_and_transparency_planes() {
        let bytes = encode();
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 4, 0]);
        let mut end = 6 + 16 * SIZES.len();
        for (index, size) in SIZES.into_iter().enumerate() {
            let directory = 6 + index * 16;
            assert_eq!(&bytes[directory..directory + 2], &[size as u8; 2]);
            assert_eq!(&bytes[directory + 4..directory + 8], &[1, 0, 32, 0]);
            let offset = u32_at(&bytes, directory + 12) as usize;
            let length = u32_at(&bytes, directory + 8) as usize;
            assert_eq!(offset, end);
            let frame = &bytes[offset..offset + length];
            assert_eq!(u32_at(frame, 0), 40);
            assert_eq!(u32_at(frame, 4), size as u32);
            assert_eq!(u32_at(frame, 8), (size * 2) as u32);
            let color_end = 40 + size * size * 4;
            let pixels = &frame[40..color_end];
            assert!(pixels.chunks_exact(4).any(|p| p[3] == 255));
            assert!(pixels.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255));
            for (pixel_index, pixel) in pixels.chunks_exact(4).enumerate() {
                assert_eq!(&pixel[..3], &[RGB[2], RGB[1], RGB[0]]);
                let y = pixel_index / size;
                let x = pixel_index % size;
                let bit = frame[color_end + y * size.div_ceil(32) * 4 + x / 8] & (0x80 >> (x % 8));
                assert_eq!(bit != 0, pixel[3] == 0);
            }
            end = offset + length;
        }
        assert_eq!(end, bytes.len());
    }

    #[cfg(windows)]
    #[test]
    fn windows_loads_embedded_icon_id_one_at_every_resolution() {
        use windows::Win32::Foundation::{FreeLibrary, HINSTANCE};
        use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW};
        use windows::Win32::System::LibraryLoader::{
            FindResourceW, GetModuleHandleW, LOAD_LIBRARY_AS_DATAFILE,
            LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW, LoadResource, LockResource,
            SizeofResource,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyIcon, GetIconInfo, HICON, ICONINFO, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW,
        };
        use windows::core::PCWSTR;

        // The default checks the linked test executable. After release build,
        // the same test can inspect that exact PE without launching the player.
        let external = std::env::var_os("KYOUKO_ICON_VERIFY_EXE");
        unsafe {
            let module = if let Some(path) = &external {
                let path: Vec<u16> = path
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                LoadLibraryExW(
                    PCWSTR(path.as_ptr()),
                    None,
                    LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
                )
                .expect("open executable resources without executing code")
            } else {
                GetModuleHandleW(None).unwrap()
            };
            let resource = FindResourceW(
                Some(module),
                PCWSTR(1 as *const u16),
                PCWSTR(14 as *const u16),
            );
            assert!(!resource.is_invalid(), "PE must contain RT_GROUP_ICON ID 1");
            let length = SizeofResource(Some(module), resource) as usize;
            let loaded = LoadResource(Some(module), resource).unwrap();
            let pointer = LockResource(loaded);
            assert!(!pointer.is_null());
            let group = std::slice::from_raw_parts(pointer as *const u8, length);
            assert_eq!(&group[..6], &[0, 0, 1, 0, 4, 0]);
            assert_eq!(length, 6 + 14 * SIZES.len());
            for (index, size) in SIZES.into_iter().enumerate() {
                let entry = 6 + 14 * index;
                assert_eq!(&group[entry..entry + 2], &[size as u8; 2]);
                let id = u16::from_le_bytes(group[entry + 12..entry + 14].try_into().unwrap());
                let frame = FindResourceW(
                    Some(module),
                    PCWSTR(id as *const u16),
                    PCWSTR(3 as *const u16),
                );
                assert!(
                    !frame.is_invalid(),
                    "group must reference an embedded RT_ICON"
                );
                assert_eq!(
                    SizeofResource(Some(module), frame),
                    u32_at(group, entry + 8)
                );
                let image = LoadImageW(
                    Some(HINSTANCE(module.0)),
                    PCWSTR(1 as *const u16),
                    IMAGE_ICON,
                    size as i32,
                    size as i32,
                    LR_DEFAULTCOLOR,
                )
                .expect("Windows must decode the embedded icon");
                let image = HICON(image.0);
                let mut info = ICONINFO::default();
                GetIconInfo(image, &mut info).unwrap();
                let mut bitmap = BITMAP::default();
                let result = GetObjectW(
                    info.hbmColor.into(),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some((&mut bitmap as *mut BITMAP).cast()),
                );
                let _ = DeleteObject(info.hbmColor.into());
                let _ = DeleteObject(info.hbmMask.into());
                DestroyIcon(image).unwrap();
                assert_ne!(result, 0);
                assert_eq!(
                    (bitmap.bmWidth, bitmap.bmHeight),
                    (size as i32, size as i32)
                );
            }
            if external.is_some() {
                FreeLibrary(module).unwrap();
            }
        }
    }
}
