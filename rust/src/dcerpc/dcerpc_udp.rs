/* Copyright (C) 2020 Open Information Security Foundation
 *
 * You can copy, redistribute or modify this Program under the terms of
 * the GNU General Public License version 2 as published by the Free
 * Software Foundation.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * version 2 along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA
 * 02110-1301, USA.
 */

use std::mem::transmute;

use crate::applayer::{AppLayerResult, AppLayerTxData};
use crate::core;
use crate::dcerpc::dcerpc::{
    DCERPCTransaction, DCERPCUuidEntry, DCERPC_TYPE_REQUEST, DCERPC_TYPE_RESPONSE, PFC_FIRST_FRAG,
};
use crate::dcerpc::parser;
use crate::log::*;
use std::cmp;

// Constant DCERPC UDP Header length
pub const DCERPC_UDP_HDR_LEN: i32 = 80;

#[derive(Debug)]
pub struct DCERPCHdrUdp {
    pub rpc_vers: u8,
    pub pkt_type: u8,
    pub flags1: u8,
    pub flags2: u8,
    pub drep: Vec<u8>,
    pub serial_hi: u8,
    pub objectuuid: Vec<u8>,
    pub interfaceuuid: Vec<u8>,
    pub activityuuid: Vec<u8>,
    pub server_boot: u32,
    pub if_vers: u32,
    pub seqnum: u32,
    pub opnum: u16,
    pub ihint: u16,
    pub ahint: u16,
    pub fraglen: u16,
    pub fragnum: u16,
    pub auth_proto: u8,
    pub serial_lo: u8,
}

#[derive(Debug)]
pub struct DCERPCUDPState {
    pub tx_id: u32,
    pub header: Option<DCERPCHdrUdp>,
    pub transactions: Vec<DCERPCTransaction>,
    pub fraglenleft: u16,
    pub uuid_entry: Option<DCERPCUuidEntry>,
    pub uuid_list: Vec<DCERPCUuidEntry>,
    pub de_state: Option<*mut core::DetectEngineState>,
    pub tx_data: AppLayerTxData,
}

impl DCERPCUDPState {
    pub fn new() -> DCERPCUDPState {
        return DCERPCUDPState {
            tx_id: 0,
            header: None,
            transactions: Vec::new(),
            fraglenleft: 0,
            uuid_entry: None,
            uuid_list: Vec::new(),
            de_state: None,
            tx_data: AppLayerTxData::new(),
        };
    }

    fn create_tx(&mut self, serial_no: u16) -> DCERPCTransaction {
        let mut tx = DCERPCTransaction::new();
        let endianness = self.get_hdr_drep_0() & 0x10;
        tx.id = self.tx_id;
        tx.call_id = serial_no as u32;
        tx.endianness = endianness;
        self.tx_id += 1;
        tx
    }


    fn evaluate_serial_no(&mut self) -> u16 {
        let mut serial_no: u16;
        let mut serial_hi: u8 = 0;
        let mut serial_lo: u8 = 0;
        let endianness = self.get_hdr_drep_0();
        if let Some(ref hdr) = &self.header {
            serial_hi = hdr.serial_hi;
            serial_lo = hdr.serial_lo;
        }
        if endianness & 0x10 == 0 {
            serial_no = serial_lo as u16;
            serial_no = serial_no.rotate_left(8) | serial_hi as u16;
        } else {
            serial_no = serial_hi as u16;
            serial_no = serial_no.rotate_left(8) | serial_lo as u16;
        }
        serial_no
    }

    fn find_tx(&mut self, serial_no: u16) -> Option<&mut DCERPCTransaction> {
        for tx in &mut self.transactions {
            let found = tx.call_id == (serial_no as u32);
            if found {
                return Some(tx);
            }
        }
        None
    }

    fn get_hdr_pkt_type(&self) -> Option<u8> {
        debug_validate_bug_on!(self.header.is_none());
        if let Some(ref hdr) = &self.header {
            return Some(hdr.pkt_type);
        }
        // Shouldn't happen
        None
    }

    fn get_hdr_flags1(&self) -> Option<u8> {
        debug_validate_bug_on!(self.header.is_none());
        if let Some(ref hdr) = &self.header {
            return Some(hdr.flags1);
        }
        // Shouldn't happen
        None
    }

    fn get_hdr_drep_0(&self) -> u8 {
        debug_validate_bug_on!(self.header.is_none());
        if let Some(ref hdr) = &self.header {
            return hdr.drep[0];
        }
        0
    }

    pub fn get_hdr_fraglen(&self) -> Option<u16> {
        debug_validate_bug_on!(self.header.is_none());
        if let Some(ref hdr) = &self.header {
            return Some(hdr.fraglen);
        }
        // Shouldn't happen
        None
    }

    pub fn handle_fragment_data(&mut self, input: &[u8], input_len: u16) -> u16 {
        let retval: u16;
        let hdrflags1 = self.get_hdr_flags1().unwrap_or(0);
        let fraglenleft = self.fraglenleft;
        let hdrtype = self.get_hdr_pkt_type().unwrap_or(0);
        let serial_no = self.evaluate_serial_no();
        let tx;
        if let Some(transaction) = self.find_tx(serial_no) {
            tx = transaction;
        } else {
            SCLogDebug!(
                "No transaction found matching the serial number: {:?}",
                serial_no
            );
            return 0;
        }

        // Update the stub params based on the packet type
        match hdrtype {
            DCERPC_TYPE_REQUEST => {
                retval = evaluate_stub_params(
                    input,
                    input_len,
                    hdrflags1,
                    fraglenleft,
                    &mut tx.stub_data_buffer_ts,
                    &mut tx.stub_data_buffer_len_ts,
                );
                tx.req_done = true;
                tx.frag_cnt_ts += 1;
            }
            DCERPC_TYPE_RESPONSE => {
                retval = evaluate_stub_params(
                    input,
                    input_len,
                    hdrflags1,
                    fraglenleft,
                    &mut tx.stub_data_buffer_tc,
                    &mut tx.stub_data_buffer_len_tc,
                );
                tx.resp_done = true;
                tx.frag_cnt_tc += 1;
            }
            _ => {
                SCLogDebug!("Unrecognized packet type");
                return 0;
            }
        }
        // Update the remaining fragment length
        self.fraglenleft -= retval;

        retval
    }

    pub fn process_header(&mut self, input: &[u8]) -> i32 {
        match parser::parse_dcerpc_udp_header(input) {
            Ok((leftover_bytes, header)) => {
                if header.rpc_vers != 4 {
                    SCLogDebug!("DCERPC UDP Header did not validate.");
                    return -1;
                }
                let mut uuidentry = DCERPCUuidEntry::new();
                let auuid = header.activityuuid.to_vec();
                uuidentry.uuid = auuid;
                self.uuid_list.push(uuidentry);
                self.header = Some(header);
                (input.len() - leftover_bytes.len()) as i32
            }
            Err(nom::Err::Incomplete(_)) => {
                // Insufficient data.
                SCLogDebug!("Insufficient data while parsing DCERPC request");
                -1
            }
            Err(_) => {
                // Error, probably malformed data.
                SCLogDebug!("An error occurred while parsing DCERPC request");
                -1
            }
        }
    }

    pub fn handle_input_data(&mut self, input: &[u8]) -> AppLayerResult {
        // Input length should at least be header length
        if (input.len() as i32) < DCERPC_UDP_HDR_LEN {
            return AppLayerResult::err();
        }
        // Call header parser first
        let mut parsed = self.process_header(input);
        if parsed == -1 {
            return AppLayerResult::err();
        }

        let mut input_left = input.len() as i32 - parsed;
        let fraglen = self.get_hdr_fraglen().unwrap_or(0);
        self.fraglenleft = fraglen;
        let serial_no = self.evaluate_serial_no();
        let tx = self.create_tx(serial_no);
        self.transactions.push(tx);
        // Parse rest of the body
        while parsed >= DCERPC_UDP_HDR_LEN && parsed < fraglen as i32 && input_left > 0 {
            let retval = self.handle_fragment_data(&input[parsed as usize..], input_left as u16);
            if retval > 0 && retval <= input_left as u16 {
                parsed += retval as i32;
                input_left -= retval as i32;
            } else if input_left > 0 {
                SCLogDebug!("Error parsing DCERPC UDP Fragment Data");
                parsed -= input_left;
                input_left = 0;
            }
        }
        return AppLayerResult::ok();
    }
}

fn evaluate_stub_params(
    input: &[u8], input_len: u16, hdrflags: u8, lenleft: u16, stub_data_buffer: &mut Vec<u8>,
    stub_data_buffer_len: &mut u16,
) -> u16 {
    let stub_len: u16;
    stub_len = cmp::min(lenleft, input_len);
    if stub_len == 0 {
        return 0;
    }
    // If the UDP frag is the the first frag irrespective of it being a part of
    // a multi frag PDU or not, it indicates the previous PDU's stub would
    // have been buffered and processed and we can use the buffer to hold
    // frags from a fresh request/response
    if hdrflags & PFC_FIRST_FRAG > 0 {
        *stub_data_buffer_len = 0;
    }

    let input_slice = &input[..stub_len as usize];
    stub_data_buffer.extend_from_slice(&input_slice);
    *stub_data_buffer_len += stub_len;

    stub_len
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_parse(
    _flow: *mut core::Flow, state: &mut DCERPCUDPState, _pstate: *mut std::os::raw::c_void,
    input: *const u8, input_len: u32, _data: *mut std::os::raw::c_void, _flags: u8,
) -> AppLayerResult {
    if input_len > 0 && input != std::ptr::null_mut() {
        let buf = build_slice!(input, input_len as usize);
        return state.handle_input_data(buf);
    }
    AppLayerResult::err()
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_state_free(state: *mut std::os::raw::c_void) {
    let _drop: Box<DCERPCUDPState> = unsafe { transmute(state) };
}

#[no_mangle]
pub unsafe extern "C" fn rs_dcerpc_udp_state_new() -> *mut std::os::raw::c_void {
    let state = DCERPCUDPState::new();
    let boxed = Box::new(state);
    transmute(boxed)
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_state_transaction_free(
    _state: *mut std::os::raw::c_void, _tx_id: u64,
) {
    // do nothing
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_tx_detect_state(
    vtx: *mut std::os::raw::c_void,
) -> *mut core::DetectEngineState {
    let dce_state = cast_pointer!(vtx, DCERPCUDPState);
    match dce_state.de_state {
        Some(ds) => ds,
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_set_tx_detect_state(
    vtx: *mut std::os::raw::c_void, de_state: *mut core::DetectEngineState,
) -> u8 {
    let dce_state = cast_pointer!(vtx, DCERPCUDPState);
    dce_state.de_state = Some(de_state);
    0
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_tx_data(
    tx: *mut std::os::raw::c_void)
    -> *mut AppLayerTxData
{
    let tx = cast_pointer!(tx, DCERPCUDPState);
    return &mut tx.tx_data;
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_tx(
    state: *mut std::os::raw::c_void, _tx_id: u64,
) -> *mut DCERPCUDPState {
    let dce_state = cast_pointer!(state, DCERPCUDPState);
    dce_state
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_tx_cnt(_state: *mut std::os::raw::c_void) -> u8 {
    1
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_alstate_progress(
    _tx: *mut std::os::raw::c_void, _direction: u8,
) -> u8 {
    0
}

#[no_mangle]
pub extern "C" fn rs_dcerpc_udp_get_alstate_progress_completion_status(_direction: u8) -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use crate::applayer::AppLayerResult;
    use crate::dcerpc::dcerpc_udp::DCERPCUDPState;

    #[test]
    fn test_process_header_udp_incomplete_hdr() {
        let request: &[u8] = &[
            0x04, 0x00, 0x08, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x4a, 0x9f, 0x4d,
            0x1c, 0x7d, 0xcf, 0x11,
        ];

        let mut dcerpcudp_state = DCERPCUDPState::new();
        assert_eq!(-1, dcerpcudp_state.process_header(request));
    }

    #[test]
    fn test_process_header_udp_perfect_hdr() {
        let request: &[u8] = &[
            0x04, 0x00, 0x08, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x4a, 0x9f, 0x4d,
            0x1c, 0x7d, 0xcf, 0x11, 0x86, 0x1e, 0x00, 0x20, 0xaf, 0x6e, 0x7c, 0x57, 0x86, 0xc2,
            0x37, 0x67, 0xf7, 0x1e, 0xd1, 0x11, 0xbc, 0xd9, 0x00, 0x60, 0x97, 0x92, 0xd2, 0x6c,
            0x79, 0xbe, 0x01, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xff, 0xff, 0xff, 0xff, 0x68, 0x00, 0x00, 0x00, 0x0a, 0x00,
        ];
        let mut dcerpcudp_state = DCERPCUDPState::new();
        assert_eq!(80, dcerpcudp_state.process_header(request));
    }

    #[test]
    fn test_handle_fragment_data_udp_no_body() {
        let request: &[u8] = &[
            0x04, 0x00, 0x08, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x4a, 0x9f, 0x4d,
            0x1c, 0x7d, 0xcf, 0x11, 0x86, 0x1e, 0x00, 0x20, 0xaf, 0x6e, 0x7c, 0x57, 0x86, 0xc2,
            0x37, 0x67, 0xf7, 0x1e, 0xd1, 0x11, 0xbc, 0xd9, 0x00, 0x60, 0x97, 0x92, 0xd2, 0x6c,
            0x79, 0xbe, 0x01, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xff, 0xff, 0xff, 0xff, 0x68, 0x00, 0x00, 0x00, 0x0a, 0x00,
        ];
        let mut dcerpcudp_state = DCERPCUDPState::new();
        assert_eq!(80, dcerpcudp_state.process_header(request));
        assert_eq!(
            0,
            dcerpcudp_state.handle_fragment_data(request, request.len() as u16)
        );
    }

    #[test]
    fn test_handle_input_data_udp_full_body() {
        let request: &[u8] = &[
            0x04, 0x00, 0x2c, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa0, 0x01, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x3f, 0x98,
            0xf0, 0x5c, 0xd9, 0x63, 0xcc, 0x46, 0xc2, 0x74, 0x51, 0x6c, 0x8a, 0x53, 0x7d, 0x6f,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
            0xff, 0xff, 0xff, 0xff, 0x70, 0x05, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x06, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x24, 0x58, 0xfd, 0xcc, 0x45,
            0x64, 0x49, 0xb0, 0x70, 0xdd, 0xae, 0x74, 0x2c, 0x96, 0xd2, 0x60, 0x5e, 0x0d, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x70, 0x5e, 0x0d, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x7c, 0x5e, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
            0x80, 0x96, 0xf1, 0xf1, 0x2a, 0x4d, 0xce, 0x11, 0xa6, 0x6a, 0x00, 0x20, 0xaf, 0x6e,
            0x72, 0xf4, 0x0c, 0x00, 0x00, 0x00, 0x4d, 0x41, 0x52, 0x42, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x0d, 0xf0, 0xad, 0xba, 0x00, 0x00, 0x00, 0x00, 0xa8, 0xf4,
            0x0b, 0x00, 0x10, 0x09, 0x00, 0x00, 0x10, 0x09, 0x00, 0x00, 0x4d, 0x45, 0x4f, 0x57,
            0x04, 0x00, 0x00, 0x00, 0xa2, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x38, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x00, 0xe0, 0x08,
            0x00, 0x00, 0xd8, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x10, 0x08, 0x00,
            0xcc, 0xcc, 0xcc, 0xcc, 0xc8, 0x00, 0x00, 0x00, 0x4d, 0x45, 0x4f, 0x57, 0xd8, 0x08,
            0x00, 0x00, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc4, 0x28, 0xcd, 0x00, 0x64, 0x29, 0xcd, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0xb9, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0xab, 0x01, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0xa5, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
            0xa6, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46, 0xa4, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x46, 0xad, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0xaa, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x07, 0x00, 0x00, 0x00, 0x60, 0x00,
            0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
            0x20, 0x00, 0x00, 0x00, 0x28, 0x06, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, 0x50, 0x00, 0x00, 0x00,
            0x4f, 0xb6, 0x88, 0x20, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, 0x48, 0x00, 0x00, 0x00, 0x07, 0x00,
            0x66, 0x00, 0x06, 0x09, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x46, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x19, 0x0c, 0x00,
            0x58, 0x00, 0x00, 0x00, 0x05, 0x00, 0x06, 0x00, 0x01, 0x00, 0x00, 0x00, 0x70, 0xd8,
            0x98, 0x93, 0x98, 0x4f, 0xd2, 0x11, 0xa9, 0x3d, 0xbe, 0x57, 0xb2, 0x00, 0x00, 0x00,
            0x32, 0x00, 0x31, 0x00, 0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, 0x80, 0x00,
            0x00, 0x00, 0x0d, 0xf0, 0xad, 0xba, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x43, 0x14, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x4d, 0x45, 0x4f, 0x57,
            0x04, 0x00, 0x00, 0x00, 0xc0, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x3b, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x81, 0xc5, 0x17, 0x03, 0x80, 0x0e, 0xe9, 0x4a,
            0x99, 0x99, 0xf1, 0x8a, 0x50, 0x6f, 0x7a, 0x85, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc,
            0xcc, 0xcc, 0x30, 0x00, 0x00, 0x00, 0x78, 0x00, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xd8, 0xda, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x2f,
            0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x46, 0x00, 0x58, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, 0x10, 0x00, 0x00, 0x00,
            0x30, 0x00, 0x2e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x10, 0x08, 0x00, 0xcc, 0xcc, 0xcc, 0xcc,
            0x68, 0x00, 0x00, 0x00, 0x0e, 0x00, 0xff, 0xff, 0x68, 0x8b, 0x0b, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfe, 0x02, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xfe, 0x02, 0x00, 0x00, 0x5c, 0x00, 0x5c, 0x00, 0x31, 0x00,
            0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00,
            0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x31, 0x00,
            0x31, 0x00, 0x31, 0x00, 0x31, 0x00, 0x9d, 0x13, 0x00, 0x01, 0xcc, 0xe0, 0xfd, 0x7f,
            0xcc, 0xe0, 0xfd, 0x7f, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
            0x90, 0x90,
        ];
        let mut dcerpcudp_state = DCERPCUDPState::new();
        assert_eq!(
            AppLayerResult::ok(),
            dcerpcudp_state.handle_input_data(request)
        );
        assert_eq!(0, dcerpcudp_state.fraglenleft);
        assert_eq!(
            1392,
            dcerpcudp_state.transactions[0].stub_data_buffer_len_ts
        );
    }
}