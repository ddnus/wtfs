use crc32fast::Hasher;
use glob::glob;

use crate::{cvt, flate};
use crate::error::Error;
use crate::state::{self, State};
use std::fmt::Display;
use std::sync::Mutex;
use std::time::SystemTime;
use std::vec;

// 操作标识
const OP_ADD: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_CLEAN: u8 = 3;

const HEADER_LEN: u8 = 7;

const STYPE_FULL: u8 = 4;
const STYPE_FIRST: u8 = 1;
const STYPE_MIDDLE: u8 = 2;
const STYPE_LAST: u8 = 3;


// payload
//    1      4      n        8
// +----+--------+------+---------+
// | op | offset | data | version |
// +----+--------+------+---------+
//
pub struct Payload {
    op: u8,
    offset: u32,
    data: Vec<u8>,
    version: u64,
}

impl Payload {
    fn new(op: u8, offset: u32, data: &Vec<u8>, version: u64) -> Self {
        Payload {
            op,
            offset,
            data: data.to_vec(),
            version,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(self.op);
        buf.extend(self.offset.to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf.extend(self.version.to_le_bytes());
        buf
    }
}
// entry data struct
//    4          2       1       n      
// +-------+----------+------+---------+
// | crc32 | data_len | type | payload |
// +-------+----------+------+---------+
//  7 + n

struct Header {
    crc32: u32, // the data offet in the store file
    dlen: u16,  // data length
    stype: u8,  // storage type  
}

struct Entry {
    header: Header,
    payload: Vec<u8>,
}

impl Entry {
    fn new(stype: u8, payload: &Vec<u8>) -> Self {
        let header: Header = Header {
            crc32: Self::checksum(payload),
            dlen: payload.len() as u16,
            stype,
        };

        Entry {
            header,
            payload: payload.to_vec(),
        }
    }

    fn to_header(buf: &Vec<u8>) -> Header {
        Header {
            crc32: cvt::case_buf_to_u32(&buf[..4].to_vec()),
            dlen: cvt::case_buf_to_u16(&buf[4..6].to_vec()),
            stype: buf[6],
        }
    }

    fn decode(buf: &Vec<u8>) -> Result<Entry, Error> {
        let header = Self::to_header(&buf);
        let data_end_offset = (header.dlen + HEADER_LEN as u16) as usize;
        let entry = Entry {
            header: header,
            payload: buf[HEADER_LEN as usize..data_end_offset].to_vec(),
        };

        if Self::checksum(&entry.payload) == entry.header.crc32 {
            return Err(Error::InvalidWalData);
        }

        Ok(entry)
    }

    fn checksum(buf: &Vec<u8>) -> u32 {
        let mut hasher = Hasher::new();
        hasher.update(buf);
        hasher.finalize()
    }

    fn encode(&self) -> Vec<u8> {
        let mut buf = self.to_header_buf();
        buf.extend_from_slice(&self.payload);
        buf
    }

    fn to_header_buf(&self) -> Vec<u8> {
        let mut buf = self.header.crc32.to_be_bytes().to_vec();
        buf.extend_from_slice(&self.header.dlen.to_be_bytes());
        buf.push(self.header.stype);
        buf
    }

}

const WAL_NAME: &str = "@wal";

pub struct Wal {
    seq: u64,
    path: String,

    wlock: Mutex<u8>,
    file_max_size: u32,
    rotation_live_time: u64,
    rotation_time: SystemTime,

    wlog: Wlog,

    log_version_list: Vec<u64>,
}

impl Wal {

    pub fn new(path: &str) -> Self {

        let log_version_list = Self::get_log_versions(path);

        let mut wlog = Wlog::new(path, log_version_list[log_version_list.len() - 1]);

        let version =  Wal::init_version(path, &log_version_list, &mut wlog).unwrap();
        
        let wal = Wal {
            path: path.to_string(),
            seq: version,

            wlock: Mutex::new(0),
            file_max_size: 4294967295,
            rotation_live_time: 1800,   // 30min
            rotation_time: SystemTime::now(),   // 30min

            wlog,

            log_version_list,
        };

        wal
    }

    pub fn append(&mut self, buf: &Vec<u8>) -> Result<(), Error> {
        let mut flate_buf = buf.clone();
        self.wlock.lock();

        self.seq += 1;

        flate_buf.extend_from_slice(&cvt::case_u64_to_buf(self.seq));

        self.rotation_log(flate_buf.len());

        self.wlog.append(&flate_buf)
    }

    fn rotation_log(&mut self, buf_len: usize) {

        if buf_len + self.wlog.file_size as usize > self.file_max_size as usize  || 
            (self.rotation_live_time > 0 &&
            SystemTime::now().duration_since(self.rotation_time).unwrap().as_secs() > self.rotation_live_time) {

            self.log_version_list.push(self.seq);

            self.wlog.close();
            
            self.wlog = Wlog::new(&self.path, self.seq);
        }

    }

    fn build_log_name<T: Display>(&self, index: T) -> String {
        state::build_path(&self.path, 
            &format!("{}-{}", WAL_NAME, index))
    }

    fn get_log_versions(path: &str) -> Vec<u64> {
        let log_glob_path = state::build_path(path, 
            &format!("{}-*", WAL_NAME));

        let globs = glob(&log_glob_path)
                .expect("Failed to read glob pattern");

        let mut list = vec![];
        for entry in globs {
            match entry {
                Ok(path) => {
                    let index_opt = path.file_name()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.split('-').last())
                        .and_then(|last| last.parse::<u64>().ok());
                    if let Some(idx) = index_opt {
                        list.push(idx);
                    }
                },
                Err(e) => println!("{:?}", e),
            }
        }

        list.sort();

        if list.len() == 0 {
            list.push(0);
        }

        list
    }

    fn init_version(path: &str, log_version_list: &Vec<u64>, active_wlog: &mut Wlog) -> Result<u64, Error> {
        let length = log_version_list.len();
        
        if length > 1 {
            if active_wlog.file_size == 0 {
                let latest_log = log_version_list[length - 2];
                return Wlog::new(path, latest_log).get_latest_version();
            }
        } else if length == 1 {
            if active_wlog.file_size == 0 {
                return Ok(0);
            }
        }

        return active_wlog.get_latest_version();
    }

    fn read_range(&mut self, min_version: u64, f: fn(buf: Vec<u8>, version: u64)) {
        if self.wlog.version == min_version {
            self.wlog.read_all(f);
        } else {
            for item in self.log_version_list.clone() {
                if item < min_version {
                    continue;
                }
                let mut wlog = Wlog::new(&self.path, item);
                wlog.read_all(f);
            }
        }
    }

    fn truncate_all(&mut self) {
        for version in self.log_version_list.clone() {
            if self.wlog.version == version {
                self.wlog.delete();
            } else {
                Wlog::new(&self.path, version).delete();
            }
        }

        let new_wal = Self::new(&self.path);

        self.path = new_wal.path;
        self.seq = new_wal.seq;
        self.wlock = new_wal.wlock;
        self.file_max_size = new_wal.file_max_size;
        self.rotation_live_time = new_wal.rotation_live_time;
        self.rotation_time = new_wal.rotation_time;
        self.wlog = new_wal.wlog;
        self.log_version_list = new_wal.log_version_list;
    }

}

struct Wlog {
    state: Box<dyn State>,
    version: u64,
    page_size: u32,
    file_size: u32,
}

impl Wlog {
    fn new(path: &str, seq: u64) -> Self {
        
        let log_file = state::build_path(path, &format!("{}-{}", WAL_NAME, seq));
        let state_handle = state::new(&log_file);
        let size = state_handle.meta().unwrap().size as u32;
        Self {
            state: state_handle,
            version: seq,
            page_size: 32768,   // 32kb
            file_size: size,
        }
    }

    pub fn close(&mut self) {
    }

    pub fn append(&mut self, buf: &Vec<u8>) -> Result<(), Error> {

        let mut flate_buf = buf.clone();

        let mut entry_bufs = vec![];

        let mut stype: u8 = 0;

        let mut active_size = self.file_size;

        let mut data_len = flate_buf.len();

        loop {
            let left_page_size = (self.page_size - active_size % self.page_size) as usize;

            if left_page_size > data_len {
                
                if stype == 0 {
                    stype = STYPE_FULL;
                } else if stype > 0 {
                    stype = STYPE_LAST;
                }

                let entry_buf = Entry::new(stype, &flate_buf).encode();
                entry_bufs.extend_from_slice(&entry_buf);

                active_size += data_len as u32;

                break;
                
            } else {
                if stype == 0 {
                    stype = STYPE_FIRST;
                } else if stype > 0 {
                    stype = STYPE_MIDDLE;
                }

                let entry_buf = Entry::new(stype, &flate_buf[..left_page_size].to_vec()).encode();

                entry_bufs.extend_from_slice(&entry_buf);

                flate_buf = flate_buf[left_page_size..].to_vec();

                data_len -= left_page_size as usize;

                active_size += left_page_size as u32;

            }
        }

        if let Ok(_) = self.state.append(&entry_bufs) {
            self.file_size += entry_bufs.len() as u32;
            return Ok(());
        }

        Err(Error::AppendWalDataFailed)
    }

    pub fn read_all(&mut self, f: fn(buf: Vec<u8>, version: u64)) {
        let mut pos = 0;
        let mut left_buf: Vec<u8> = vec![]; 
        loop {
            let mut page_buf = vec![0u8; self.page_size as usize];
            let fetch_res = self.state.get(pos, &mut page_buf);

            match fetch_res {
                Ok(get_size) => {

                    left_buf.extend_from_slice(&page_buf[..]);

                    self.handle_page(&mut left_buf, f);

                    // is the last page
                    if get_size < self.page_size as usize {
                        break;
                    }
                },
                _ => {},
            }
            pos += self.page_size as usize;
        }
    }

    fn handle_page(&mut self, page_buf: &mut Vec<u8>, f: fn(buf: Vec<u8>, version: u64)) {
        
        let mut remain_buf: Vec<u8> = page_buf.clone();

        let mut chunk_datas = vec![];
        let mut page_chunk_size: usize = 0;

        loop {
            if remain_buf.len() <= HEADER_LEN as usize {
                break;
            }

            let header = Entry::to_header(&remain_buf);

            let chunk_size = (HEADER_LEN as u16 + header.dlen) as usize;
            let chunk_data = remain_buf[HEADER_LEN as usize..chunk_size].to_vec();

            page_chunk_size += chunk_size;

            if Entry::checksum(&chunk_data) != header.crc32 {
                break;
            }

            remain_buf.drain(..chunk_size);

            chunk_datas.extend_from_slice(&chunk_data);

            if header.stype == STYPE_FULL || header.stype == STYPE_LAST {

                page_buf.drain(..page_chunk_size);

                let version_pos = chunk_datas.len()-8;

                f(chunk_datas[..version_pos].to_vec(), cvt::case_buf_to_u64(&chunk_datas[version_pos..].to_vec()));

                chunk_datas = vec![];

            }

        }

    }

    fn get_latest_version(&mut self) -> Result<u64, Error> {
        let mut version_buf = [0u8; 8];
        if let Ok(size) = self.state.get_from_end(-8, &mut version_buf) {
            if size == 8 {
                return Ok(cvt::case_buf_to_u64(&version_buf.to_vec()));
            } else {
                panic!("Read version size: {} error", size);
            }
        } else {
            panic!("Init version faild");
        }

    }

    fn delete(&mut self) {
        self.state.remove();
    }

}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_bitmap_path(path: &str) -> String {
        #[cfg(target_family = "unix")]
        return "/tmp/terra/tests/".to_string() + path;
        #[cfg(target_family = "windows")]
        return std::env::temp_dir().to_str().unwrap().to_string() + "/terra/tests/" + path;
    }

    #[test]
    fn test_add() {
        let mut wal = Wal::new(&tmp_bitmap_path("wal"));
        wal.truncate_all();
        let app_result = wal.append(&vec![3u8; 20]);
        assert!(app_result.is_ok());

        wal.read_range(0, |buf, version| {
            assert_eq!(buf, vec![3u8; 20]);
            assert_eq!(version, 1);
        })

    }
}
