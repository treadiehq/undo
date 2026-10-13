/** One request at a time, with the newest pending value replacing older ones.
 * Unlike debouncing, continuous dragging still makes progress. */
export function createPreviewQueue<T>(
  load: (request: T, signal: AbortSignal) => Promise<void>,
  onError: (error: unknown, request: T) => void,
  interval = 80,
) {
  let pending: T | undefined
  let timer: ReturnType<typeof setTimeout> | null = null
  let active: AbortController | null = null
  let lastStarted = -Infinity

  function pump() {
    if (active || timer || pending === undefined) return
    timer = setTimeout(async () => {
      timer = null
      const request = pending!
      pending = undefined
      const controller = new AbortController()
      active = controller
      lastStarted = Date.now()
      try {
        await load(request, controller.signal)
      } catch (error) {
        if (!controller.signal.aborted) onError(error, request)
      } finally {
        // A cancelled request must not clear a newer request's controller.
        if (active === controller) {
          active = null
          pump()
        }
      }
    }, Math.max(0, interval - (Date.now() - lastStarted)))
  }

  return {
    schedule(request: T) {
      pending = request
      pump()
    },
    cancel() {
      if (timer) clearTimeout(timer)
      timer = null
      pending = undefined
      active?.abort()
      active = null
      lastStarted = -Infinity
    },
  }
}
