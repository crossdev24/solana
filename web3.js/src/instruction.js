// @flow

import * as BufferLayout from 'buffer-layout';

import * as Layout from './layout';

/**
 * @typedef {Object} InstructionType
 * @property (index} The Instruction index (from solana upstream program)
 * @property (BufferLayout} The BufferLayout to use to build data
 */
export type InstructionType = {|
  index: number,
  layout: typeof BufferLayout,
|};

/**
 * Populate a buffer of instruction data using an InstructionType
 */
export function encodeData(type: InstructionType, fields: Object): Buffer {
  const allocLength =
    type.layout.span >= 0 ? type.layout.span : Layout.getAlloc(type, fields);
  const data = Buffer.alloc(allocLength);
  const layoutFields = Object.assign({instruction: type.index}, fields);
  type.layout.encode(layoutFields, data);
  return data;
}
