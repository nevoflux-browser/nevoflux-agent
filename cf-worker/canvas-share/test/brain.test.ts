import { describe, it, expect } from 'vitest';
import { isValidNbrainMagic } from '../src/utils/validation';

describe('isValidNbrainMagic', () => {
  it('accepts a buffer beginning with NBRN', () => {
    const buf = new Uint8Array([0x4e, 0x42, 0x52, 0x4e, 0x01, 0xaa]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(true);
  });
  it('rejects the canvas NFEB magic', () => {
    const buf = new Uint8Array([0x4e, 0x46, 0x45, 0x42]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(false);
  });
  it('rejects a buffer shorter than 4 bytes', () => {
    const buf = new Uint8Array([0x4e, 0x42, 0x52]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(false);
  });
  it('rejects an empty buffer', () => {
    expect(isValidNbrainMagic(new ArrayBuffer(0))).toBe(false);
  });
});
