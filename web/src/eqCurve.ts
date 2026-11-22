/**
 * Biquad magnitude response for the console's EQ chart.
 *
 * Same RBJ cookbook formulas the firmware's `loudness.rs` uses, designed at
 * the playback rate (48 kHz) and evaluated on the digital response, so the
 * drawn curve is the curve the device applies — not a hand-drawn sketch.
 *
 * What it canNOT show: the bass guard and the three compressors of the
 * EchoMuse voicing are gain-law stages, not filters, so they are not part of
 * the magnitude response. The card labels the chart as "校正 + 你的 EQ".
 */

export const FS = 48000

export interface Band {
  freq: number
  gain_db: number
  q: number
}

interface Coeffs {
  b0: number
  b1: number
  b2: number
  a1: number
  a2: number
}

function peaking(freq: number, q: number, gainDb: number): Coeffs {
  const a = Math.pow(10, gainDb / 40)
  const w0 = (2 * Math.PI * freq) / FS
  const cw = Math.cos(w0)
  const sw = Math.sin(w0)
  const alpha = sw / (2 * q)
  const b0 = 1 + alpha * a
  const b1 = -2 * cw
  const b2 = 1 - alpha * a
  const a0 = 1 + alpha / a
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: -2 * cw / a0, a2: (1 - alpha / a) / a0 }
}

function lowShelf(freq: number, gainDb: number): Coeffs {
  const a = Math.pow(10, gainDb / 40)
  const w0 = (2 * Math.PI * freq) / FS
  const cw = Math.cos(w0)
  const sw = Math.sin(w0)
  const twoSqrtAAlpha = 2 * Math.sqrt(a) * sw * Math.SQRT1_2
  const b0 = a * (a + 1 - (a - 1) * cw + twoSqrtAAlpha)
  const b1 = 2 * a * (a - 1 - (a + 1) * cw)
  const b2 = a * (a + 1 - (a - 1) * cw - twoSqrtAAlpha)
  const a0 = a + 1 + (a - 1) * cw + twoSqrtAAlpha
  const a1 = -2 * (a - 1 + (a + 1) * cw)
  const a2 = a + 1 + (a - 1) * cw - twoSqrtAAlpha
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

function highShelf(freq: number, gainDb: number): Coeffs {
  const a = Math.pow(10, gainDb / 40)
  const w0 = (2 * Math.PI * freq) / FS
  const cw = Math.cos(w0)
  const sw = Math.sin(w0)
  const twoSqrtAAlpha = 2 * Math.sqrt(a) * sw * Math.SQRT1_2
  const b0 = a * (a + 1 + (a - 1) * cw + twoSqrtAAlpha)
  const b1 = -2 * a * (a - 1 + (a + 1) * cw)
  const b2 = a * (a + 1 + (a - 1) * cw - twoSqrtAAlpha)
  const a0 = a + 1 - (a - 1) * cw + twoSqrtAAlpha
  const a1 = 2 * (a - 1 - (a + 1) * cw)
  const a2 = a + 1 - (a - 1) * cw - twoSqrtAAlpha
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

function highPass(freq: number, q: number): Coeffs {
  const w0 = (2 * Math.PI * freq) / FS
  const cw = Math.cos(w0)
  const sw = Math.sin(w0)
  const alpha = sw / (2 * q)
  const b0 = (1 + cw) / 2
  const b1 = -(1 + cw)
  const b2 = (1 + cw) / 2
  const a0 = 1 + alpha
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: (-2 * cw) / a0, a2: (1 - alpha) / a0 }
}

function magnitudeDb(c: Coeffs, freq: number): number {
  const w = (2 * Math.PI * freq) / FS
  const cw = Math.cos(w)
  const sw = Math.sin(w)
  const cw2 = Math.cos(2 * w)
  const sw2 = Math.sin(2 * w)
  const numRe = c.b0 + c.b1 * cw + c.b2 * cw2
  const numIm = -(c.b1 * sw + c.b2 * sw2)
  const denRe = 1 + c.a1 * cw + c.a2 * cw2
  const denIm = -(c.a1 * sw + c.a2 * sw2)
  const num = Math.hypot(numRe, numIm)
  const den = Math.hypot(denRe, denIm) || 1e-12
  return 20 * Math.log10(Math.max(num / den, 1e-9))
}

/** The voicing's fixed linear part: 80 Hz LR4 high-pass, a 65 Hz notch and the
 * driver correction. Mirrors `EM_*` in the firmware's loudness.rs. */
const VOICING_HPF = highPass(80, Math.SQRT1_2)

/** Combined response of the fixed voicing and the user's peaking bands. */
export function responseDb(voicing: Band[], userBands: Band[], freq: number): number {
  let db = 2 * magnitudeDb(VOICING_HPF, freq) // LR4 = the Butterworth section twice
  for (const b of voicing) {
    db += magnitudeDb(peaking(b.freq, b.q, b.gain_db), freq)
  }
  for (const b of userBands) {
    db += magnitudeDb(peaking(b.freq, b.q, b.gain_db), freq)
  }
  return db
}

/** The user's own contribution alone (what the card's sliders change). */
export function userResponseDb(userBands: Band[], freq: number): number {
  let db = 0
  for (const b of userBands) {
    db += magnitudeDb(peaking(b.freq, b.q, b.gain_db), freq)
  }
  return db
}

/** Log-spaced frequencies from 20 Hz to 16 kHz for the chart's x axis. */
export function logFrequencies(count = 160): number[] {
  const lo = Math.log10(20)
  const hi = Math.log10(16000)
  return Array.from({ length: count }, (_, i) => Math.pow(10, lo + ((hi - lo) * i) / (count - 1)))
}
