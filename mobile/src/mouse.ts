/** Mouse buttons as a terminal reports them: the low bits of the button byte. */
export const MOUSE_LEFT = 0;
export const MOUSE_WHEEL_UP = 64;
export const MOUSE_WHEEL_DOWN = 65;

/** The largest column or row the legacy encoding carries here. Its bytes are
 * 32 + n, and the pane's input travels as text: a byte past 127 would be sent
 * as two bytes of UTF-8, which the program would read as a different report. */
const LEGACY_MAX = 95;

/**
 * A mouse report for a program that asked for the mouse.
 *
 * `x` and `y` are the 1-based column and row; anything below 1 is taken as 1.
 * With `sgr` (the program set `?1006`) the report is `CSI < b ; x ; y M` for a
 * press and the same with `m` for a release, so the release names its button.
 * Without it, the legacy X10 form `CSI M` + three bytes of 32 + n, where a
 * release is button 3 (it cannot say which button let go) and the position
 * stops at 95.
 *
 * Cases, for checking by hand (no test runner in this package):
 *   mouseReport(0, 1, 1, true, true)       "\x1b[<0;1;1M"
 *   mouseReport(0, 10, 5, false, true)     "\x1b[<0;10;5m"
 *   mouseReport(64, 3, 4, true, true)      "\x1b[<64;3;4M"
 *   mouseReport(0, 0, -2, true, true)      "\x1b[<0;1;1M"       clamped up to 1
 *   mouseReport(0, 1, 1, true, false)      "\x1b[M !!"          32, 33, 33
 *   mouseReport(0, 1, 1, false, false)     "\x1b[M#!!"          release is 3
 *   mouseReport(65, 2, 2, true, false)     "\x1b[Ma\"\""        97, 34, 34
 *   mouseReport(0, 200, 120, true, false)  "\x1b[M \x7f\x7f"    capped at 95
 *   mouseReport(0, 200, 120, true, true)   "\x1b[<0;200;120M"   SGR has no cap
 */
export function mouseReport(button: number, x: number, y: number, press: boolean, sgr: boolean): string {
  const col = Math.max(1, Math.floor(x));
  const row = Math.max(1, Math.floor(y));
  if (sgr) return `\x1b[<${button};${col};${row}${press ? "M" : "m"}`;
  const code = press ? button : (button & ~3) | 3;
  return `\x1b[M${String.fromCharCode(32 + code, 32 + Math.min(col, LEGACY_MAX), 32 + Math.min(row, LEGACY_MAX))}`;
}
