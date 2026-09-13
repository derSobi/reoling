use crate::bc::header_codec::{read_header, write_header};
use crate::bc::model::{
    has_payload_offset, Bc, BcBody, BcHeader, BcMeta, LegacyMsg, ModernMsg, MSG_ID_LOGIN,
};
use crate::crypto::EncryptionProtocol;
use crate::Error;

pub fn write_bc(bc: &Bc, enc: &EncryptionProtocol) -> Vec<u8> {
    // Symmetric with `read_bc`'s override: the whole login exchange (both
    // directions) stays on BCEncrypt even once AES has been negotiated —
    // real firmware expects our modern-login request encoded that way too.
    let enc = match enc {
        EncryptionProtocol::Aes { .. } if bc.meta.msg_id == MSG_ID_LOGIN => {
            &EncryptionProtocol::BcEncrypt
        }
        other => other,
    };
    let (body_bytes, payload_offset) = match &bc.body {
        BcBody::Legacy(LegacyMsg::LoginUpgrade) => (Vec::new(), None),
        BcBody::Modern(modern) => {
            let mut body = Vec::new();
            let mut offset = 0u32;
            if let Some(ext) = &modern.extension_xml {
                let encrypted_ext = enc.encrypt(0, ext);
                offset = encrypted_ext.len() as u32;
                body.extend_from_slice(&encrypted_ext);
            }
            if let Some(payload) = &modern.payload {
                let encrypted_payload = enc.encrypt(offset, payload);
                body.extend_from_slice(&encrypted_payload);
            }
            // Only classes that actually carry the payload_offset word
            // (0x6414, 0x0000) get `Some`; e.g. 0x6614 (the encrypted reply
            // to legacy login) is a modern body with no offset word at all —
            // giving it one here desyncs write_header/read_header.
            let payload_offset = if has_payload_offset(bc.meta.class) {
                Some(offset)
            } else {
                None
            };
            (body, payload_offset)
        }
    };

    let header = BcHeader {
        msg_id: bc.meta.msg_id,
        body_len: body_bytes.len() as u32,
        channel_id: bc.meta.channel_id,
        stream_type: bc.meta.stream_type,
        msg_num: bc.meta.msg_num,
        response_code: bc.meta.response_code,
        class: bc.meta.class,
        payload_offset,
    };

    let mut out = write_header(&header);
    out.extend_from_slice(&body_bytes);
    out
}

pub fn read_bc(buf: &[u8], enc: &EncryptionProtocol) -> crate::Result<Option<(Bc, usize)>> {
    let Some((header, header_len)) = read_header(buf)? else {
        return Ok(None);
    };
    let total_len = header_len + header.body_len as usize;
    if buf.len() < total_len {
        return Ok(None);
    }
    let body = &buf[header_len..total_len];

    // The header (never encrypted) is enough on its own to tell us which
    // login-phase (`MSG_ID_LOGIN`) reply we're looking at, and real
    // hardware doesn't decrypt those the way a naive reading of the
    // negotiated `enc` would suggest — confirmed 2026-09-13 by comparing
    // against `neolink`'s `de.rs` (never copied, only read) after our own
    // AES-negotiated login failed to decrypt on real hardware:
    // - The very first login reply (the one carrying the nonce, in
    //   response to our request's `response_code` upper byte 0xdc) always
    //   comes back with upper byte 0xdd. Its lower byte is the encryption
    //   the device actually picked: 0x00 means genuinely unencrypted,
    //   anything else (0x01 BCEncrypt, 0x12 AES, ...) means BCEncrypt —
    //   never AES, since the AES key itself is derived from the nonce
    //   this very message is delivering, so it can't already be AES.
    // - Every other `MSG_ID_LOGIN` reply (e.g. the modern login's
    //   DeviceInfo) is *also* BCEncrypt whenever the negotiated protocol
    //   is AES: real firmware appears to keep the whole login exchange on
    //   BCEncrypt and only starts using AES for messages after login
    //   completes.
    let effective_enc = if header.msg_id == MSG_ID_LOGIN {
        if (header.response_code >> 8) & 0xff == 0xdd {
            if header.response_code & 0xff == 0 {
                EncryptionProtocol::Unencrypted
            } else {
                EncryptionProtocol::BcEncrypt
            }
        } else {
            match enc {
                EncryptionProtocol::Aes { .. } => EncryptionProtocol::BcEncrypt,
                other => other.clone(),
            }
        }
    } else {
        enc.clone()
    };
    let enc = &effective_enc;

    let meta = BcMeta {
        msg_id: header.msg_id,
        channel_id: header.channel_id,
        stream_type: header.stream_type,
        msg_num: header.msg_num,
        response_code: header.response_code,
        class: header.class,
    };

    let bc_body = match header.payload_offset {
        None if header.class == 0x6514 => BcBody::Legacy(LegacyMsg::LoginUpgrade),
        None => {
            // e.g. class 0x6614: modern body, no extension, whole body is payload.
            BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: if body.is_empty() {
                    None
                } else {
                    Some(enc.decrypt(0, body))
                },
            })
        }
        Some(offset) => {
            let offset = offset as usize;
            if offset > body.len() {
                return Err(Error::ProtocolError(format!(
                    "payload_offset {offset} beyond body_len {}",
                    body.len()
                )));
            }
            let extension_xml = if offset == 0 {
                None
            } else {
                Some(enc.decrypt(0, &body[..offset]))
            };
            let payload = if offset == body.len() {
                None
            } else {
                Some(enc.decrypt(offset as u32, &body[offset..]))
            };
            BcBody::Modern(ModernMsg {
                extension_xml,
                payload,
            })
        }
    };

    Ok(Some((
        Bc {
            meta,
            body: bc_body,
        },
        total_len,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::model::*;
    use crate::crypto::EncryptionProtocol;

    #[test]
    fn round_trips_a_legacy_login_upgrade() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0xdc12,
                class: 0x6514,
            },
            body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
        };
        let bytes = write_bc(&bc, &EncryptionProtocol::Unencrypted);
        assert_eq!(bytes.len(), 20); // header only, no body

        let (parsed, consumed) = read_bc(&bytes, &EncryptionProtocol::Unencrypted)
            .unwrap()
            .unwrap();
        assert_eq!(consumed, 20);
        assert_eq!(parsed, bc);
    }

    #[test]
    fn legacy_login_upgrade_matches_a_real_captured_request_byte_for_byte() {
        // Captured 2026-09-13 from the official Windows client logging
        // into a real Home Hub Pro NVR over TCP:9000 (LAN) — see
        // .plans/docu/Wireshark/Reolink-ALL-Login_only_with_Home_Hub_Pro.pcapng,
        // frame 129. This is the exact request our own P2P/UDP client
        // (bug #15) was getting wrong: `response_code` was an invented
        // 0xdc02 instead of the real 0xdc12, and `channel_id` was being
        // set to the target camera's channel instead of 0 (login is a
        // host-level operation, even on an NVR — see `bug #14`/`#15` in
        // .plans/reolink-linux-project.md).
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: 0xdc12,
                class: 0x6514,
            },
            body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
        };
        let bytes = write_bc(&bc, &EncryptionProtocol::Unencrypted);
        assert_eq!(
            bytes,
            [
                0xf0, 0xde, 0xbc, 0x0a, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x12, 0xdc, 0x14, 0x65,
            ]
        );
    }

    #[test]
    fn round_trips_a_modern_message_with_payload_through_bc_encrypt() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };
        let enc = EncryptionProtocol::BcEncrypt;
        let bytes = write_bc(&bc, &enc);
        let (parsed, consumed) = read_bc(&bytes, &enc).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, bc);
    }

    #[test]
    fn login_messages_stay_on_bc_encrypt_even_once_aes_is_negotiated() {
        // Confirmed against a real capture, 2026-09-13 (a fresh Windows VM
        // install, login only, no prior state): the client's actual
        // LoginUser request and the device's DeviceInfo reply both decoded
        // as valid XML *only* under BCEncrypt, never AES, despite AES
        // being the negotiated protocol for the rest of the session. Every
        // other observed message id after login failed to decode as XML
        // under either Unencrypted or BCEncrypt, consistent with those
        // being genuinely AES-encrypted.
        let key = [0x42u8; 16];
        let aes = EncryptionProtocol::Aes { key };
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body><DeviceInfo/></body>".to_vec()),
            }),
        };

        // Encoding with the *negotiated* Aes protocol must still produce
        // BcEncrypt bytes on the wire for this msg_id.
        let bytes = write_bc(&bc, &aes);
        let bc_encrypt_bytes = write_bc(&bc, &EncryptionProtocol::BcEncrypt);
        assert_eq!(bytes, bc_encrypt_bytes);

        // Decoding those bytes while still passing the negotiated Aes
        // protocol must recover the original message (not garbage), i.e.
        // read_bc applies the same override on the way in.
        let (parsed, _) = read_bc(&bytes, &aes).unwrap().unwrap();
        assert_eq!(parsed, bc);

        // A non-login message id is unaffected: it really does use Aes.
        let mut non_login = bc.clone();
        non_login.meta.msg_id = 999;
        let non_login_bytes = write_bc(&non_login, &aes);
        assert_ne!(non_login_bytes, write_bc(&non_login, &EncryptionProtocol::BcEncrypt));
    }

    #[test]
    fn returns_none_until_the_full_body_has_arrived() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };
        let enc = EncryptionProtocol::Unencrypted;
        let bytes = write_bc(&bc, &enc);
        assert!(read_bc(&bytes[..bytes.len() - 1], &enc).unwrap().is_none());
    }
}
