// Forgiving search: "bigwig", "big wigs", "dbm", "weak aura", "elv" and small typos ("questy") all find what you meant.
'use strict';

const Search = (() => {
  const norm = (s) =>
    String(s || '')
      .replace(/([a-z])([A-Z])/g, '$1 $2')
      .toLowerCase()
      .normalize('NFKD')
      .replace(/[̀-ͯ]/g, '')
      .replace(/[^a-z0-9]+/g, ' ')
      .trim();
  const compact = (s) => norm(s).replace(/ /g, '');
  const initials = (s) => norm(s).split(' ').filter(Boolean).map((w) => w[0]).join('');

  function subsequence(needle, hay) {
    let i = 0;
    for (const c of hay) if (c === needle[i]) i++;
    return i === needle.length;
  }

  // edit distance, giving up past 1
  function near(a, b) {
    if (Math.abs(a.length - b.length) > 1) return false;
    let i = 0, j = 0, edits = 0;
    while (i < a.length && j < b.length) {
      if (a[i] === b[j]) { i++; j++; continue; }
      if (++edits > 1) return false;
      if (a.length > b.length) i++;
      else if (b.length > a.length) j++;
      else { i++; j++; }
    }
    return edits + (a.length - i) + (b.length - j) <= 1;
  }

  /** Prepare a thing to search: name, plus other text (author, summary, folders). */
  function index(name, others) {
    return {
      name: norm(name),
      nameC: compact(name),
      words: norm(name).split(' ').filter(Boolean),
      acr: initials(name),
      rest: norm(others.join(' ')),
      restC: compact(others.join(' ')),
    };
  }

  /** 0 = no match; higher is better. */
  function score(ix, query) {
    const q = norm(query);
    if (!q) return 1;
    const qc = q.replace(/ /g, '');
    const tokens = q.split(' ');
    if (ix.nameC === qc) return 100;
    if (ix.nameC.startsWith(qc)) return 90;
    if (ix.nameC.includes(qc)) return 80;
    if (qc.length >= 2 && ix.acr.startsWith(qc)) return 75;
    const all = ix.name + ' ' + ix.rest;
    if (tokens.every((t) => ix.name.includes(t))) return 70;
    if (tokens.every((t) => all.includes(t))) return 60;
    if (qc.length >= 4 && ix.restC.includes(qc)) return 55;
    if (tokens.every((t) => t.length < 4 ? all.includes(t) : ix.words.some((w) => near(t, w) || near(t, w.slice(0, t.length))))) return 40;
    if (qc.length >= 3 && subsequence(qc, ix.nameC)) return 25;
    return 0;
  }

  return { index, score, norm };
})();
