/**
 * @brief Example C-based BPF program that prints out the parameters
 * passed to it
 */
#include <solana_sdk.h>

extern bool entrypoint(const uint8_t *input) {
  SolKeyedAccounts ka[1];
  uint64_t ka_len;
  const uint8_t *data;
  uint64_t data_len;
  SolClusterInfo info;

  sol_log(__FILE__);

  if (!sol_deserialize(input, ka, SOL_ARRAY_SIZE(ka), &ka_len, &data, &data_len, &info)) {
    return false;
  }
  sol_log_64(info.tick_height, 0, 0, 0, 0);

  // Log the provided account keys and instruction input data.  In the case of
  // the no-op program, no account keys or input data are expected but real
  // programs will have specific requirements so they can do their work.
  sol_log_params(ka, ka_len, data, data_len);
  return true;
}
