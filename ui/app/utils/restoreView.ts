import type { DiffPayload, DiffTarget, RecoveryEntryView, TimelineItem } from '~/types'

/** Keep the recorded order, but only render paths the restore would change. */
export function affectedItems(items: readonly TimelineItem[], entries: readonly RecoveryEntryView[]) {
  const paths = new Set(entries.map((entry) => entry.path))
  return items.flatMap((item) => {
    const seen = new Set<string>()
    const files = item.files.flatMap((file) => {
      // A rename can restore the old path as well as remove the new path.
      return [file.path, file.old_path].flatMap((path) => {
        if (!path || !paths.has(path) || seen.has(path)) return []
        seen.add(path)
        return [{ ...file, path, old_path: null }]
      })
    })
    return files.length ? [{ ...item, files, file_count: files.length }] : []
  })
}

export function nextRestoreTarget(items: readonly TimelineItem[], current: DiffTarget | null): DiffTarget | null {
  const sameItem = items.find((item) => item.id === current?.itemId)
  const sameFile = sameItem?.files.find((file) => file.path === current?.file.path)
  if (sameItem && sameFile) return { itemId: sameItem.id, file: sameFile }
  const item = sameItem ?? items[0]
  return item?.files[0] ? { itemId: item.id, file: item.files[0] } : null
}

export function sameDiffContent(a: DiffPayload | null, b: DiffPayload | null) {
  return a?.path === b?.path && a?.change === b?.change && a?.binary === b?.binary
    && JSON.stringify(a?.hunks) === JSON.stringify(b?.hunks)
}
