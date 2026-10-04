/**
 * Just enough YAML to emit a Tanod configuration, and no more.
 *
 * A general YAML serialiser is a dependency and an attack surface; this file
 * emits one known shape. Every string is double-quoted and escaped, so no
 * value can ever be reinterpreted as structure — which matters because route
 * ids and paths come from a build manifest rather than from a person.
 */

/** Double-quote and escape a scalar so it cannot break out of its position. */
export function quote(value) {
  const escaped = String(value)
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\t/g, '\\t')
    // Anything else non-printable becomes an escape rather than a raw byte in
    // a config file somebody has to read.
    .replace(/[\x00-\x1f\x7f]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);
  return `"${escaped}"`;
}

/** A list of strings on one line: `["a", "b"]`. */
export function inlineList(values) {
  return `[${values.map(quote).join(', ')}]`;
}

/** An append-only buffer of output lines. */
export class Lines {
  #out = [];

  raw(line = '') {
    this.#out.push(line);
    return this;
  }

  comment(text, indent = 0) {
    const pad = ' '.repeat(indent);
    for (const line of String(text).split('\n')) {
      this.#out.push(line ? `${pad}# ${line}` : `${pad}#`);
    }
    return this;
  }

  toString() {
    return `${this.#out.join('\n')}\n`;
  }
}
