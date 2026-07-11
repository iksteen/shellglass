// shellglass embed — the STABLE public embedding API. One line where the
// terminal should appear:
//
//   <script src="https://hub.example.com/embed.js"
//           data-src="https://hub.example.com/s/demo"></script>
//
// The script replaces itself with an iframe onto the session's embed page
// (`?embed`), class "shellglass-view", default 100% wide × 24em tall. Size it
// with a style attribute on the script tag (carried onto the iframe) or by
// styling .shellglass-view. For dynamic insertion or markup control there is
// also an element form (document.currentScript is null there, so place it
// yourself):
//
//   <shellglass-view src="https://hub.example.com/s/demo"></shellglass-view>
//
// A CLASSIC script on purpose: document.currentScript doesn't exist in
// modules, and classic cross-origin loads need no CORS. Everything live —
// rendering, reconnects, upgrades, the offline state — happens inside the
// frame, so this file has no version coupling to the viewer and must stay
// tiny and backward compatible: never remove or repurpose `data-src`, the
// element name, or its `src` attribute.
(() => {
  const embedUrl = (src) => {
    try {
      const u = new URL(src, location.href);
      u.searchParams.set("embed", "");
      return u.href;
    } catch {
      return null; // malformed src: leave the frame blank rather than throw
    }
  };

  // One-liner form: replace the script tag itself with the iframe. The size
  // defaults live in a zero-specificity :where() rule, so ANY host stylesheet
  // rule on .shellglass-view overrides them — no !important needed; a style
  // attribute on the script tag (carried onto the iframe) beats both, as
  // inline style does.
  const me = document.currentScript;
  if (me && me.dataset.src) {
    if (!document.getElementById("shellglass-embed-css")) {
      const st = document.createElement("style");
      st.id = "shellglass-embed-css";
      st.textContent =
        ":where(iframe.shellglass-view){display:block;border:0;width:100%;height:24em}";
      document.head.append(st);
    }
    const f = document.createElement("iframe");
    f.className = "shellglass-view";
    f.title = "shellglass terminal";
    const style = me.getAttribute("style");
    if (style) f.style.cssText = style;
    const href = embedUrl(me.dataset.src);
    if (href) f.src = href;
    me.replaceWith(f);
  }

  // Element form (also usable when this file is loaded as a module).
  if (customElements.get("shellglass-view")) return; // loaded twice: keep the first
  class ShellglassView extends HTMLElement {
    static observedAttributes = ["src"];
    connectedCallback() {
      if (!this.shadowRoot) {
        const root = this.attachShadow({ mode: "open" });
        const style = document.createElement("style");
        style.textContent =
          ":host{display:block;height:24em}" +
          "iframe{border:0;display:block;width:100%;height:100%}";
        this.frame = document.createElement("iframe");
        this.frame.title = "shellglass terminal";
        root.append(style, this.frame);
      }
      this.point();
    }
    attributeChangedCallback() {
      this.point();
    }
    point() {
      const src = this.getAttribute("src");
      if (!this.frame || !src) return;
      const href = embedUrl(src);
      if (href && this.frame.src !== href) this.frame.src = href;
    }
  }
  customElements.define("shellglass-view", ShellglassView);
})();
