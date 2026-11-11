// 报表链四环节各自的验收测试。每个测试只盯一个模块，互不牵连。
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { loadLines } from '../src/loader.js';
import { clean } from '../src/cleaner.js';
import { toPercent } from '../src/transformer.js';
import { rankScores } from '../src/ranker.js';

test('loadLines keeps every line including the last', () => {
  // 破损基线：loadLines 丢了最后一行，['a','b'] 而不是 ['a','b','c']。
  assert.deepEqual(loadLines('a\nb\nc'), ['a', 'b', 'c']);
});

test('clean trims both sides', () => {
  // 破损基线：clean 只去了左边，右边空白原样保留。
  assert.equal(clean('  x  '), 'x');
});

test('toPercent multiplies by one hundred', () => {
  // 破损基线：toPercent 忘了乘 100，0.25 原样返回。
  assert.equal(toPercent(0.25), 25);
});

test('rankScores descends by score', () => {
  // 破损基线：rankScores 升序返回，名次应降序 [3, 2, 1]。
  assert.deepEqual(rankScores([3, 1, 2]), [3, 2, 1]);
});
