use std::io;
use accountant::Accountant;
use event::{Event, PublicKey, Signature};
use log::{Entry, Sha256Hash};
use std::net::UdpSocket;
use bincode::{deserialize, serialize};

pub struct AccountantSkel {
    pub obj: Accountant,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Transfer {
        from: PublicKey,
        to: PublicKey,
        val: u64,
        sig: Signature,
    },
    GetBalance {
        key: PublicKey,
    },
    GetEntries {
        last_id: Option<Sha256Hash>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Balance { key: PublicKey, val: u64 },
    Confirmed { sig: Signature },
    Entries { entries: Vec<Entry<u64>> },
}

impl AccountantSkel {
    pub fn new(obj: Accountant) -> Self {
        AccountantSkel { obj }
    }

    pub fn process_request(self: &mut Self, msg: Request) -> Option<Response> {
        match msg {
            Request::Transfer { from, to, val, sig } => {
                let event = Event::Transaction {
                    from,
                    to,
                    data: val,
                    sig,
                };
                if let Err(err) = self.obj.process_event(event) {
                    println!("Transfer error: {:?}", err);
                }
                None
            }
            Request::GetBalance { key } => {
                let val = self.obj.get_balance(&key).unwrap();
                Some(Response::Balance { key, val })
            }
            Request::GetEntries { .. } => Some(Response::Entries { entries: vec![] }),
        }
    }

    /// UDP Server that forwards messages to Accountant methods.
    pub fn serve(self: &mut Self, addr: &str) -> io::Result<()> {
        let socket = UdpSocket::bind(addr)?;
        let mut buf = vec![0u8; 1024];
        loop {
            //println!("skel: Waiting for incoming packets...");
            let (_sz, src) = socket.recv_from(&mut buf)?;

            // TODO: Return a descriptive error message if deserialization fails.
            let req = deserialize(&buf).expect("deserialize request");

            if let Some(resp) = self.process_request(req) {
                socket.send_to(&serialize(&resp).expect("serialize response"), &src)?;
            }
        }
    }
}
