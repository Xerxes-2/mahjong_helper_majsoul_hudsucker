use anyhow::{anyhow, bail, ensure, Result};
use base64::prelude::*;
use bytes::Bytes;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, SerializeOptions};
use serde_json::{value::Serializer, Value as JsonValue};
use std::{collections::HashMap, sync::Arc};

use crate::SETTINGS;

const SERIALIZE_OPTIONS: SerializeOptions = SerializeOptions::new()
    .skip_default_fields(false)
    .use_proto_field_name(true);

#[derive(Debug)]
pub enum MessageType {
    Notify = 1,
    Request = 2,
    Response = 3,
}

#[derive(Debug)]
pub struct LiqiMessage {
    pub id: usize,
    pub msg_type: MessageType,
    pub method_name: Arc<str>,
    pub data: JsonValue,
}

#[derive(Debug)]
pub struct Parser {
    total: usize,
    respond_type: HashMap<usize, (Arc<str>, MessageDescriptor)>,
    proto_json: &'static JsonValue,
    pool: &'static DescriptorPool,
}

pub fn dyn_to_json(msg: DynamicMessage) -> Result<JsonValue> {
    Ok(msg.serialize_with_options(Serializer, &SERIALIZE_OPTIONS)?)
}

impl Parser {
    pub fn new() -> Self {
        Self {
            total: 0,
            respond_type: HashMap::new(),
            proto_json: &SETTINGS.proto_json,
            pool: &SETTINGS.desc,
        }
    }

    pub fn parse(&mut self, buf: &[u8]) -> Result<LiqiMessage> {
        let msg_type_byte = buf[0];
        ensure!(
            (1..=3).contains(&msg_type_byte),
            "Invalid message type: {}",
            msg_type_byte
        );
        let msg_type = match msg_type_byte {
            1 => MessageType::Notify,
            2 => MessageType::Request,
            3 => MessageType::Response,
            _ => unreachable!(),
        };
        let method_name: Arc<str>;
        let mut data_obj: JsonValue;
        let msg_id: usize;
        match msg_type {
            MessageType::Notify => {
                let (method, data) = buf_to_method_data(&buf[1..])?;
                let method_name_str = String::from_utf8(method.into())?;
                method_name = Arc::from(method_name_str);
                let method_name_list: Vec<&str> = method_name.split('.').collect();
                let message_name = method_name_list[2];
                let message_type = self
                    .pool
                    .get_message_by_name(&to_fqn(message_name))
                    .ok_or(anyhow!("Invalid message type: {}", message_name))?;
                let dyn_msg = DynamicMessage::decode(message_type, data)?;
                data_obj = dyn_to_json(dyn_msg)?;
                if let Some(b64) = data_obj.get("data") {
                    let action_name = data_obj
                        .get("name")
                        .and_then(|n| n.as_str())
                        .ok_or(anyhow!("name field invalid"))?;
                    let b64 = b64.as_str().unwrap_or_default();
                    let action_obj = decode_action(action_name, b64, &self.pool)?;
                    data_obj
                        .as_object_mut()
                        .ok_or(anyhow!("data is not an object"))?
                        .insert("data".to_string(), action_obj);
                }
                msg_id = self.total;
            }
            MessageType::Request => {
                // little endian, msg_id = unpack("<H", buf[1:3])[0]
                msg_id = u16::from_le_bytes([buf[1], buf[2]]) as usize;
                let (method, data) = buf_to_method_data(&buf[3..])?;
                assert!(msg_id < 1 << 16);
                let method_name_str = String::from_utf8(method.into())?;
                method_name = Arc::from(method_name_str);
                let method_name_list: Vec<&str> = method_name.split('.').collect();
                let lq = method_name_list[1];
                let service = method_name_list[2];
                let rpc = method_name_list[3];
                let proto_domain =
                    &self.proto_json["nested"][lq]["nested"][service]["methods"][rpc];
                let req_type_name = &proto_domain["requestType"]
                    .as_str()
                    .ok_or(anyhow!("Invalid request type"))?;
                let req_type = self
                    .pool
                    .get_message_by_name(&to_fqn(req_type_name))
                    .ok_or(anyhow!("Invalid request type: {}", req_type_name))?;
                let dyn_msg = DynamicMessage::decode(req_type, data)?;
                data_obj = dyn_to_json(dyn_msg)?;
                let res_type_name = proto_domain["responseType"]
                    .as_str()
                    .ok_or(anyhow!("Invalid response type"))?;
                let resp_type = self
                    .pool
                    .get_message_by_name(&to_fqn(res_type_name))
                    .ok_or(anyhow!("Invalid response type: {}", res_type_name))?;
                self.respond_type
                    .insert(msg_id, (method_name.clone(), resp_type));
            }
            MessageType::Response => {
                msg_id = u16::from_le_bytes([buf[1], buf[2]]) as usize;
                let (method, data) = buf_to_method_data(&buf[3..])?;
                assert!(method.is_empty());
                let resp_type: MessageDescriptor;
                (method_name, resp_type) = self
                    .respond_type
                    .remove(&msg_id)
                    .ok_or(anyhow!("No corresponding request"))?;
                let dyn_msg = DynamicMessage::decode(resp_type, data)?;
                data_obj = dyn_to_json(dyn_msg)?;
            }
        }
        self.total += 1;
        Ok(LiqiMessage {
            id: msg_id,
            msg_type,
            method_name,
            data: data_obj,
        })
    }
}

pub fn to_fqn(method_name: &str) -> String {
    format!("lq.{}", method_name)
}

struct Block {
    _id: usize,
    _blk_type: usize,
    data: Bytes,
    _begin: usize,
}

pub fn decode_action(name: &str, data: &str, pool: &DescriptorPool) -> Result<JsonValue> {
    let mut decoded = BASE64_STANDARD.decode(data)?;
    wtf_decode(&mut decoded);
    let action_type = pool
        .get_message_by_name(&to_fqn(name))
        .ok_or(anyhow!("Invalid action type: {}", name))?;
    let action_msg = DynamicMessage::decode(action_type, Bytes::from(decoded))?;
    dyn_to_json(action_msg)
}

fn buf_to_method_data(buf: &[u8]) -> Result<(Bytes, Bytes)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    let l = buf.len();
    while i < l {
        let begin = i;
        let blk_type = (buf[i] & 0x07) as usize;
        let id = (buf[i] >> 3) as usize;
        i += 1;
        let data: Bytes;
        match blk_type {
            0 => {
                let int = parse_var_int(buf, &mut i);
                // convert int to bytes
                data = int.to_be_bytes().to_vec().into();
            }
            2 => {
                let len = parse_var_int(buf, &mut i);
                data = Bytes::copy_from_slice(&buf[i..i + len]);
                i += len;
            }
            _ => bail!("Invalid block type: {}", blk_type),
        }
        blocks.push(Block {
            _id: id,
            _blk_type: blk_type,
            data,
            _begin: begin,
        });
    }
    ensure!(
        blocks.len() == 2,
        "Invalid number of blocks: {}",
        blocks.len()
    );
    let data_block = blocks.pop().ok_or(anyhow!("No data block"))?;
    let method_block = blocks.pop().ok_or(anyhow!("No method block"))?;
    Ok((method_block.data, data_block.data))
}

fn parse_var_int(buf: &[u8], p: &mut usize) -> usize {
    let mut data = 0;
    let mut shift = 0;
    for b in buf.iter().skip(*p) {
        data += ((*b & 0x7f) as usize) << shift;
        *p += 1;
        shift += 7;
        if b >> 7 == 0 {
            break;
        }
    }
    data
}

fn wtf_decode(data: &mut [u8]) {
    const KEYS: [usize; 9] = [0x84, 0x5E, 0x4E, 0x42, 0x39, 0xA2, 0x1F, 0x60, 0x1C];
    let d = data.len();
    KEYS.iter()
        .cycle()
        .zip(data.iter_mut())
        .enumerate()
        .map(|(i, (key, b))| (((23 ^ d) + 5 * i + key) & 255, b))
        .for_each(|(k, b)| *b ^= k as u8);
}
