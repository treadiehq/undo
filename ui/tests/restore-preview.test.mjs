import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createPreviewQueue } from '../app/utils/previewQueue.ts'
import { affectedItems, nextRestoreTarget, sameDiffContent } from '../app/utils/restoreView.ts'

function deferred() {
  let resolve
  const promise = new Promise((done) => { resolve = done })
  return { promise, resolve }
}

test('dragging keeps making progress and coalesces intermediate positions', async () => {
  const firstStarted = deferred()
  const firstDone = deferred()
  const latestStarted = deferred()
  const calls = []
  const queue = createPreviewQueue(async (value) => {
    calls.push(value)
    if (value === 1) {
      firstStarted.resolve()
      await firstDone.promise
    } else latestStarted.resolve()
  }, assert.fail, 0)
  queue.schedule(1)
  await firstStarted.promise
  for (let value = 2; value <= 100; value++) queue.schedule(value)
  assert.deepEqual(calls, [1], 'do not start parallel requests while dragging')
  firstDone.resolve()
  await latestStarted.promise
  assert.deepEqual(calls, [1, 100], 'render progress, then use the latest queued position')
  queue.cancel()
})

test('changing context cancels old work without interrupting the new request', async () => {
  const oldStarted = deferred()
  const oldDone = deferred()
  const newStarted = deferred()
  const newDone = deferred()
  const finalStarted = deferred()
  let oldSignal
  const calls = []
  const queue = createPreviewQueue(async (value, signal) => {
    calls.push(value)
    if (value === 'old') {
      oldSignal = signal
      oldStarted.resolve()
      await oldDone.promise
    } else if (value === 'new') {
      newStarted.resolve()
      await newDone.promise
    } else finalStarted.resolve()
  }, assert.fail, 0)
  queue.schedule('old')
  await oldStarted.promise
  queue.schedule('discard')
  queue.cancel()
  assert.equal(oldSignal.aborted, true)
  queue.schedule('new')
  await newStarted.promise
  oldDone.resolve()
  await Promise.resolve()
  queue.schedule('final')
  newDone.resolve()
  await finalStarted.promise
  assert.deepEqual(calls, ['old', 'new', 'final'])
  queue.cancel()
})

test('a failed preview does not stop later slider updates', async () => {
  const failed = deferred()
  const succeeded = deferred()
  const queue = createPreviewQueue(async (value) => {
    if (value === 1) throw new Error('missing snapshot')
    succeeded.resolve()
  }, (error) => { assert.equal(error.message, 'missing snapshot'); failed.resolve() }, 0)
  queue.schedule(1)
  await failed.promise
  queue.schedule(2)
  await succeeded.promise
  queue.cancel()
})

const files = ['a.ts', 'b.ts', 'c.ts'].map((path) => ({ path, old_path: null }))
const items = [{ id: 'run', files, file_count: 3 }, { id: 'older', files: [{ path: 'old.ts' }], file_count: 1 }]
const entries = (paths) => paths.map((path) => ({ path, action: 'WRITE' }))

test('only affected files and nonempty activity groups remain, in recorded order', () => {
  const visible = affectedItems(items, entries(['c.ts', 'a.ts']))
  assert.deepEqual(visible.map((item) => item.id), ['run'])
  assert.deepEqual(visible[0].files.map((file) => file.path), ['a.ts', 'c.ts'])
  assert.equal(visible[0].file_count, 2)
  assert.equal(items[0].files.length, 3, 'never mutate the recorded timeline')
  assert.deepEqual(affectedItems(items, []), [])
})

test('selection stays on its file until it leaves, then picks a visible file', () => {
  const current = { itemId: 'run', file: files[1] }
  assert.equal(nextRestoreTarget(affectedItems(items, entries(['a.ts', 'b.ts'])), current).file.path, 'b.ts')
  assert.equal(nextRestoreTarget(affectedItems(items, entries(['c.ts', 'old.ts'])), current).file.path, 'c.ts')
  assert.equal(nextRestoreTarget([], current), null)
  assert.equal(nextRestoreTarget(affectedItems(items, entries(['a.ts'])), null).file.path, 'a.ts')
})

test('rename recovery includes both affected paths without duplicates', () => {
  const renamed = [{ id: 'run', files: [{ path: 'new.ts', old_path: 'old.ts' }, { path: 'old.ts' }] }]
  assert.deepEqual(affectedItems(renamed, entries(['old.ts', 'new.ts']))[0].files.map((file) => file.path), ['new.ts', 'old.ts'])
})

test('moving across identical content does not restart the diff animation', () => {
  const diff = { path: 'a.ts', change: 'modified', binary: false, new_timestamp: 10, hunks: [{ lines: [{ text: 'before' }] }] }
  assert.equal(sameDiffContent(diff, { ...diff, new_timestamp: 20 }), true)
  assert.equal(sameDiffContent(diff, { ...diff, hunks: [{ lines: [{ text: 'after' }] }] }), false)
  assert.equal(sameDiffContent(diff, null), false)
})
