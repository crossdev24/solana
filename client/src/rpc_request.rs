use serde_json::{json, Value};
use std::{error, fmt};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RpcEpochInfo {
    /// The current epoch
    pub epoch: u64,

    /// The current slot, relative to the start of the current epoch
    pub slot_index: u64,

    /// The number of slots in this epoch
    pub slots_in_epoch: u64,

    /// The absolute current slot
    pub absolute_slot: u64,
}

#[derive(Debug, PartialEq)]
pub enum RpcRequest {
    ConfirmTransaction,
    DeregisterNode,
    FullnodeExit,
    GetAccountInfo,
    GetBalance,
    GetClusterNodes,
    GetEpochInfo,
    GetGenesisBlockhash,
    GetInflation,
    GetNumBlocksSinceSignatureConfirmation,
    GetProgramAccounts,
    GetRecentBlockhash,
    GetSignatureStatus,
    GetSlot,
    GetSlotLeader,
    GetStorageTurn,
    GetStorageTurnRate,
    GetSlotsPerSegment,
    GetStoragePubkeysForSlot,
    GetTransactionCount,
    GetVersion,
    GetVoteAccounts,
    RegisterNode,
    RequestAirdrop,
    SendTransaction,
    SignVote,
    GetMinimumBalanceForRentExemption,
}

impl RpcRequest {
    pub(crate) fn build_request_json(&self, id: u64, params: Option<Value>) -> Value {
        let jsonrpc = "2.0";
        let method = match self {
            RpcRequest::ConfirmTransaction => "confirmTransaction",
            RpcRequest::DeregisterNode => "deregisterNode",
            RpcRequest::FullnodeExit => "fullnodeExit",
            RpcRequest::GetAccountInfo => "getAccountInfo",
            RpcRequest::GetBalance => "getBalance",
            RpcRequest::GetClusterNodes => "getClusterNodes",
            RpcRequest::GetEpochInfo => "getEpochInfo",
            RpcRequest::GetGenesisBlockhash => "getGenesisBlockhash",
            RpcRequest::GetInflation => "getInflation",
            RpcRequest::GetNumBlocksSinceSignatureConfirmation => {
                "getNumBlocksSinceSignatureConfirmation"
            }
            RpcRequest::GetProgramAccounts => "getProgramAccounts",
            RpcRequest::GetRecentBlockhash => "getRecentBlockhash",
            RpcRequest::GetSignatureStatus => "getSignatureStatus",
            RpcRequest::GetSlot => "getSlot",
            RpcRequest::GetSlotLeader => "getSlotLeader",
            RpcRequest::GetStorageTurn => "getStorageTurn",
            RpcRequest::GetStorageTurnRate => "getStorageTurnRate",
            RpcRequest::GetSlotsPerSegment => "getSlotsPerSegment",
            RpcRequest::GetStoragePubkeysForSlot => "getStoragePubkeysForSlot",
            RpcRequest::GetTransactionCount => "getTransactionCount",
            RpcRequest::GetVersion => "getVersion",
            RpcRequest::GetVoteAccounts => "getVoteAccounts",
            RpcRequest::RegisterNode => "registerNode",
            RpcRequest::RequestAirdrop => "requestAirdrop",
            RpcRequest::SendTransaction => "sendTransaction",
            RpcRequest::SignVote => "signVote",
            RpcRequest::GetMinimumBalanceForRentExemption => "getMinimumBalanceForRentExemption",
        };
        let mut request = json!({
           "jsonrpc": jsonrpc,
           "id": id,
           "method": method,
        });
        if let Some(param_string) = params {
            request["params"] = param_string;
        }
        request
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    RpcRequestError(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid")
    }
}

impl error::Error for RpcError {
    fn description(&self) -> &str {
        "invalid"
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        // Generic error, underlying cause isn't tracked.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_request_json() {
        let test_request = RpcRequest::GetAccountInfo;
        let addr = json!(["deadbeefXjn8o3yroDHxUtKsZZgoy4GPkPPXfouKNHhx"]);
        let request = test_request.build_request_json(1, Some(addr.clone()));
        assert_eq!(request["method"], "getAccountInfo");
        assert_eq!(request["params"], addr,);

        let test_request = RpcRequest::GetBalance;
        let request = test_request.build_request_json(1, Some(addr));
        assert_eq!(request["method"], "getBalance");

        let test_request = RpcRequest::GetEpochInfo;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "getEpochInfo");

        let test_request = RpcRequest::GetInflation;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "getInflation");

        let test_request = RpcRequest::GetRecentBlockhash;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "getRecentBlockhash");

        let test_request = RpcRequest::GetSlot;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "getSlot");

        let test_request = RpcRequest::GetTransactionCount;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "getTransactionCount");

        let test_request = RpcRequest::RequestAirdrop;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "requestAirdrop");

        let test_request = RpcRequest::SendTransaction;
        let request = test_request.build_request_json(1, None);
        assert_eq!(request["method"], "sendTransaction");
    }
}
