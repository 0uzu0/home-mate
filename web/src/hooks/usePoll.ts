/**
 * Status polling. Two rules, both from the brief:
 *   - the page polls on a fixed interval (2 s for live state);
 *   - a hidden tab does no work at all, and catches up immediately when it
 *     becomes visible again.
 *
 * The interval is torn down while hidden instead of merely skipping ticks, so
 * a background tab does not keep waking the device/core for nothing.
 */
import { onMounted, onUnmounted } from 'vue'

export function usePoll(run: () => void | Promise<void>, intervalMs = 2000) {
  let timer: number | null = null
  let wanted = false
  let running = false

  async function tick(): Promise<void> {
    if (running || document.hidden) return
    running = true
    try {
      await run()
    } finally {
      running = false
    }
  }

  function clear(): void {
    if (timer !== null) {
      window.clearInterval(timer)
      timer = null
    }
  }

  function schedule(): void {
    if (timer !== null || !wanted || document.hidden) return
    void tick()
    timer = window.setInterval(() => void tick(), intervalMs)
  }

  function onVisibility(): void {
    clear()
    if (!document.hidden) schedule()
  }

  function start(): void {
    wanted = true
    schedule()
  }

  function stop(): void {
    wanted = false
    clear()
  }

  onMounted(() => {
    document.addEventListener('visibilitychange', onVisibility)
    start()
  })
  onUnmounted(() => {
    document.removeEventListener('visibilitychange', onVisibility)
    stop()
  })

  return { start, stop, tick }
}
