import { test } from 'node:test';
import assert from 'node:assert/strict';
import { capitalize, trimAll } from '../src/utils.js';

// 这条测试在破损 fixture 上会失败：capitalize('') 抛 TypeError。
test('empty input returns empty string', () => {
  assert.equal(capitalize(''), '');
});

test('normal input capitalizes the first letter', () => {
  assert.equal(capitalize('hello'), 'Hello');
});

test('trimAll removes surrounding whitespace', () => {
  assert.equal(trimAll('  pad  '), 'pad');
});
