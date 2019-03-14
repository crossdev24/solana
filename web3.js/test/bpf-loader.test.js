// @flow

import fs from 'mz/fs';

import {
  Connection,
  BpfLoader,
  Transaction,
  sendAndConfirmTransaction,
} from '../src';
import {mockRpcEnabled} from './__mocks__/node-fetch';
import {url} from './url';
import {newAccountWithLamports} from './new-account-with-lamports';

if (!mockRpcEnabled) {
  // The default of 5 seconds is too slow for live testing sometimes
  jest.setTimeout(30000);
}

test('load BPF program', async () => {
  if (mockRpcEnabled) {
    console.log('non-live test skipped');
    return;
  }

  const connection = new Connection(url);
  const from = await newAccountWithLamports(connection, 1024);
  const data = await fs.readFile('test/fixtures/noop/noop.so');
  const programId = await BpfLoader.load(connection, from, data);
  const transaction = new Transaction().add({
    keys: [from.publicKey],
    programId,
  });
  await sendAndConfirmTransaction(connection, transaction, from);
});
