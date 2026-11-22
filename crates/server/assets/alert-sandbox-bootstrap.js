/* Custom alert sandbox bootstrap (R25). The ONLY script that runs in the frame.
 *
 * Custom alerts are HTML and CSS with fields, never code: the markup is parsed into an inert
 * document, stripped of anything that runs, navigates or fetches, and only then are the
 * viewer's words written in as text (never as markup). The frame's CSP allows this script
 * by nonce alone, so a handler or script that slips past still cannot run.
 *
 * Kept in step with the engine's copy (crates/server/assets/alert-sandbox-bootstrap.js) and
 * with the save-time check (core alert.rs `check_custom_html`).
 */
;(function () {
  'use strict'
  var root = document.getElementById('fc-custom-root')
  var styleEl = document.getElementById('fc-custom-style')
  var PARENT = (function () {
    var a = document.documentElement.getAttribute('data-parent') || ''
    if (!a) a = new URLSearchParams(location.search).get('origin') || ''
    return /^https?:\/\/[^/?#\\]+$/.test(a) ? a : ''
  })()
  if (!PARENT || !root || !styleEl) return

  var MAX_HTML = 80000
  var MAX_CSS = 80000
  var MAX_EXPANDED = 200000
  var BANNED_TAGS = {
    script: 1,
    meta: 1,
    base: 1,
    form: 1,
    iframe: 1,
    frame: 1,
    frameset: 1,
    object: 1,
    embed: 1,
    link: 1,
    applet: 1,
    noscript: 1,
    noembed: 1,
    noframes: 1,
    xmp: 1,
    plaintext: 1,
    textarea: 1,
    title: 1,
    template: 1,
    math: 1,
    foreignobject: 1,
    use: 1,
    animate: 1,
    set: 1,
    animatemotion: 1,
    animatetransform: 1,
    input: 1,
    button: 1,
    select: 1,
    audio: 1,
    video: 1,
    source: 1,
    track: 1,
  }
  var BANNED_ATTRS = { srcdoc: 1, formaction: 1, action: 1, 'http-equiv': 1, ping: 1 }
  var URL_ATTRS = {
    href: 1,
    src: 1,
    'xlink:href': 1,
    poster: 1,
    data: 1,
    background: 1,
    srcset: 1,
    cite: 1,
    longdesc: 1,
    manifest: 1,
    icon: 1,
  }
  var HOSTS = [
    'static-cdn.jtvnw.net',
    'yt3.ggpht.com',
    'files.kick.com',
    'cdn.ferrocast.com',
    'cdn.discordapp.com',
  ]
  var done = false

  function isObj(v) {
    return typeof v === 'object' && v !== null && !Array.isArray(v)
  }
  function validFieldKey(k) {
    return typeof k === 'string' && /^[a-z][a-z0-9_]{0,39}$/.test(k)
  }
  function tokenAttrOk(n) {
    return (
      n === 'alt' ||
      n === 'title' ||
      n === 'aria-label' ||
      n === 'aria-description' ||
      n === 'aria-valuetext' ||
      n.indexOf('data-') === 0
    )
  }

  /** Lowercase CSS with comments dropped and escapes decoded, as a browser reads it. */
  function normCss(css) {
    var s = css.replace(/\/\*[\s\S]*?\*\//g, '')
    s = s.replace(/\\([0-9a-fA-F]{1,6})[ \t\n\r\f]?/g, function (_, h) {
      var n = parseInt(h, 16)
      return n > 0 && n < 0x110000 ? String.fromCodePoint(n) : ''
    })
    s = s.replace(/\\([\s\S])/g, '$1')
    return s.toLowerCase()
  }
  function cssOk(css) {
    if (css.length > MAX_CSS) return false
    var l = normCss(css)
    var bad = [
      '@import',
      'expression(',
      'javascript:',
      '-moz-binding',
      'behavior:',
      'image-set(',
      'src(',
    ]
    for (var i = 0; i < bad.length; i++) if (l.indexOf(bad[i]) !== -1) return false
    var at = l.indexOf('url(')
    while (at !== -1) {
      var inner = l
        .slice(at + 4)
        .replace(/^\s+/, '')
        .replace(/^["']+/, '')
      if (inner.indexOf('data:image/') !== 0) return false
      at = l.indexOf('url(', at + 4)
    }
    return true
  }
  function urlOk(value) {
    var v = String(value).trim().toLowerCase()
    // eslint-disable-next-line no-control-regex
    if (/[\s\u0000-\u001f\u007f\\]/.test(v)) return false
    // Entities only matter where a scheme could hide; signed picture links carry &.
    if (v.indexOf('&') !== -1 && v.indexOf('https://') !== 0) return false
    if (v.charAt(0) === '#') return true
    var m = /^data:image\/(png|jpeg|gif|webp|avif);/.exec(v)
    if (m) return true
    if (v.indexOf('https://') === 0) {
      var host = v.slice(8).split(/[/?#]/)[0]
      return HOSTS.indexOf(host) !== -1
    }
    return false
  }

  function sanitize(node) {
    var kids = Array.prototype.slice.call(node.childNodes)
    for (var i = 0; i < kids.length; i++) {
      var c = kids[i]
      if (c.nodeType === 8) {
        node.removeChild(c)
        continue
      }
      if (c.nodeType !== 1) continue
      var name = (c.localName || '').toLowerCase()
      if (BANNED_TAGS[name]) {
        node.removeChild(c)
        continue
      }
      if (name === 'style') {
        if (!cssOk(c.textContent || '')) {
          node.removeChild(c)
          continue
        }
      }
      var attrs = Array.prototype.slice.call(c.attributes)
      for (var j = 0; j < attrs.length; j++) {
        var an = attrs[j].name.toLowerCase()
        var av = attrs[j].value
        var drop = false
        if (an.indexOf('on') === 0 || BANNED_ATTRS[an]) drop = true
        else if (URL_ATTRS[an]) drop = !urlOk(av)
        else if (an === 'style') drop = !cssOk('x{' + av + '}')
        if (drop) c.removeAttribute(attrs[j].name)
      }
      sanitize(c)
    }
  }

  function fillTokens(text, map, budget) {
    return text.replace(/\{\{?([a-zA-Z0-9_:]+)(?:\|([^}]*))?\}?\}/g, function (full, nm, filt) {
      if (full.indexOf('{{') === 0) return full.slice(1, -1)
      var out = ''
      if (Object.prototype.hasOwnProperty.call(map, nm) && map[nm] !== '') out = map[nm]
      else if (filt && filt.indexOf('default:') === 0)
        out = filt.slice(8).replace(/^["']|["']$/g, '')
      if (out.length > budget.left) out = out.slice(0, Math.max(0, budget.left))
      budget.left -= out.length
      return out
    })
  }
  function substitute(node, map, budget, inStyle) {
    for (var c = node.firstChild; c; c = c.nextSibling) {
      if (c.nodeType === 3) {
        if (!inStyle) c.nodeValue = fillTokens(c.nodeValue || '', map, budget)
      } else if (c.nodeType === 1) {
        var name = (c.localName || '').toLowerCase()
        var attrs = Array.prototype.slice.call(c.attributes)
        for (var j = 0; j < attrs.length; j++) {
          var an = attrs[j].name.toLowerCase()
          if (tokenAttrOk(an))
            c.setAttribute(attrs[j].name, fillTokens(attrs[j].value, map, budget))
        }
        substitute(c, map, budget, name === 'style')
      }
    }
  }

  function safeCssVal(v) {
    var s = String(v == null ? '' : v)
    if (s.length > 500 || /[\\/*;{}<>()"'`\n\r]/.test(s)) return ''
    if (/url|image-set|src|expression|@import|javascript/i.test(s)) return ''
    return s
  }

  function validate(msg) {
    if (!isObj(msg) || msg.type !== 'ferro-custom-alert') return null
    var p = msg.payload
    if (!isObj(p) || typeof p.html !== 'string' || p.html.length > MAX_HTML) return null
    if (typeof p.css !== 'string' || p.css.length > MAX_CSS || !isObj(p.vars) || !isObj(p.fields))
      return null
    return p
  }

  function render(p) {
    var map = Object.create(null)
    var k
    for (k in p.vars)
      if (Object.prototype.hasOwnProperty.call(p.vars, k) && typeof p.vars[k] === 'string')
        map[k] = p.vars[k]
    var docStyle = document.documentElement.style
    for (k in p.fields) {
      if (!Object.prototype.hasOwnProperty.call(p.fields, k) || !validFieldKey(k)) continue
      var fv = p.fields[k]
      if (typeof fv !== 'string' && typeof fv !== 'number' && typeof fv !== 'boolean') continue
      map[k] = String(fv)
      var sv = safeCssVal(fv)
      if (sv !== '') docStyle.setProperty('--field-' + k, sv)
    }
    styleEl.textContent = cssOk(p.css) ? p.css : ''
    // An inert document: nothing in it runs or loads until it is cleaned and adopted.
    var body = new DOMParser().parseFromString('<!doctype html><body>' + p.html, 'text/html').body
    sanitize(body)
    substitute(body, map, { left: MAX_EXPANDED }, false)
    while (root.firstChild) root.removeChild(root.firstChild)
    while (body.firstChild) {
      var n = body.firstChild
      body.removeChild(n)
      root.appendChild(document.importNode(n, true))
    }
  }

  function onMessage(ev) {
    if (done || ev.source !== window.parent || ev.origin !== PARENT) return
    var p = validate(ev.data)
    if (!p) return
    done = true
    window.removeEventListener('message', onMessage)
    render(p)
    window.parent.postMessage(
      { type: 'ferro-custom-alert-rendered', id: String(p.alert_id || '') },
      PARENT,
    )
  }
  window.addEventListener('message', onMessage)
  window.parent.postMessage({ type: 'ferro-custom-alert-ready' }, PARENT)
})()
