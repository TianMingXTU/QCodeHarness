import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dedupeByKey, firstBy } from '../src/dedupe.js';

// 这条测试在破损 fixture 上会失败：Map 覆盖写入，'a' 与 'b' 都被
// 后出现的同名元素挤掉。
test('duplicate keys keep the first occurrence', () => {
  const items = [
    { name: 'a', v: 1 },
    { name: 'b', v: 2 },
    { name: 'a', v: 3 },
    { name: 'b', v: 4 },
  ];
  const result = dedupeByKey(items, (item) => item.name);
  assert.deepEqual(result, [{ name: 'a', v: 1 }, { name: 'b', v: 2 }]);
});

test('unique items pass through in order', () => {
  const result = dedupeByKey([{ n: 1 }, { n: 2 }], (item) => item.n);
  assert.deepEqual(result, [{ n: 1 }, { n: 2 }]);
});

test('firstBy picks the minimum key', () => {
  const items = [
    { k: 3, x: 'a' },
    { k: 1, x: 'b' },
    { k: 2, x: 'c' },
  ];
  assert.equal(firstBy(items, (item) => item.k).x, 'b');
});
