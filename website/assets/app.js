/* Fjord docs — theme, nav, copy buttons, TOC tracking, search, highlighting. */
(function () {
  "use strict";

  /* ------------------------------------------------------------- theme --- */

  var root = document.documentElement;
  var stored = null;
  try { stored = localStorage.getItem("fjord-theme"); } catch (e) {}
  if (stored === "dark" || stored === "light") root.setAttribute("data-theme", stored);

  function currentTheme() {
    var explicit = root.getAttribute("data-theme");
    if (explicit) return explicit;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }

  var themeButton = document.querySelector(".theme");
  if (themeButton) {
    themeButton.addEventListener("click", function () {
      var next = currentTheme() === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try { localStorage.setItem("fjord-theme", next); } catch (e) {}
    });
  }

  /* --------------------------------------------------------- mobile nav --- */

  var menu = document.querySelector(".menu");
  if (menu) {
    menu.addEventListener("click", function () {
      var open = document.body.classList.toggle("nav-open");
      menu.setAttribute("aria-expanded", open ? "true" : "false");
    });
  }
  document.addEventListener("click", function (event) {
    if (!document.body.classList.contains("nav-open")) return;
    if (event.target.closest(".sidebar") || event.target.closest(".menu")) return;
    document.body.classList.remove("nav-open");
  });

  /* ------------------------------------------------------ copy a block --- */

  document.querySelectorAll("figure.code .copy").forEach(function (button) {
    button.addEventListener("click", function () {
      var code = button.closest("figure").querySelector("code");
      var text = code ? code.textContent : "";
      var done = function () {
        button.textContent = "copied";
        button.classList.add("done");
        setTimeout(function () {
          button.textContent = "copy";
          button.classList.remove("done");
        }, 1400);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, function () {});
        return;
      }
      var scratch = document.createElement("textarea");
      scratch.value = text;
      document.body.appendChild(scratch);
      scratch.select();
      try { document.execCommand("copy"); done(); } catch (e) {}
      document.body.removeChild(scratch);
    });
  });

  /* --------------------------------------------------------------- toc --- */

  var tocLinks = Array.prototype.slice.call(document.querySelectorAll(".toc a"));
  if (tocLinks.length) {
    var targets = tocLinks
      .map(function (link) { return document.getElementById(link.hash.slice(1)); })
      .filter(Boolean);

    var mark = function () {
      var best = null;
      targets.forEach(function (heading) {
        if (heading.getBoundingClientRect().top - 90 <= 0) best = heading;
      });
      tocLinks.forEach(function (link) {
        link.classList.toggle("active", best && link.hash === "#" + best.id);
      });
    };
    mark();
    var queued = false;
    window.addEventListener("scroll", function () {
      if (queued) return;
      queued = true;
      window.requestAnimationFrame(function () { queued = false; mark(); });
    }, { passive: true });
  }

  /* ------------------------------------------------------------ search --- */

  var modal = document.querySelector(".search-modal");
  var input = modal && modal.querySelector("input");
  var results = modal && modal.querySelector(".results");
  var index = null;
  var selection = 0;

  function loadIndex() {
    if (index) return Promise.resolve(index);
    return fetch("search-index.json")
      .then(function (response) { return response.json(); })
      .then(function (data) { index = data; return index; })
      .catch(function () { index = []; return index; });
  }

  function openSearch() {
    if (!modal) return;
    modal.hidden = false;
    loadIndex().then(function () { render(input.value); });
    input.focus();
    input.select();
  }

  function closeSearch() {
    if (!modal) return;
    modal.hidden = true;
  }

  function escapeHtml(text) {
    return text.replace(/[&<>"]/g, function (character) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[character];
    });
  }

  function highlight(text, needle) {
    var at = text.toLowerCase().indexOf(needle);
    if (at < 0 || !needle) return escapeHtml(text);
    return (
      escapeHtml(text.slice(0, at)) +
      "<em>" + escapeHtml(text.slice(at, at + needle.length)) + "</em>" +
      escapeHtml(text.slice(at + needle.length))
    );
  }

  function snippet(text, needle) {
    if (!needle) return text.slice(0, 120);
    var at = text.toLowerCase().indexOf(needle);
    if (at < 0) return text.slice(0, 120);
    var from = Math.max(0, at - 45);
    return (from ? "…" : "") + text.slice(from, from + 150);
  }

  function render(raw) {
    if (!results) return;
    var needle = raw.trim().toLowerCase();
    results.innerHTML = "";
    selection = 0;
    if (!needle) {
      results.innerHTML = '<li class="empty">Type to search titles and body text.</li>';
      return;
    }

    var words = needle.split(/\s+/);
    var scored = (index || [])
      .map(function (entry) {
        var title = entry.title.toLowerCase();
        var page = entry.page.toLowerCase();
        var text = (entry.text || "").toLowerCase();
        var score = 0;
        for (var i = 0; i < words.length; i++) {
          var word = words[i];
          if (!word) continue;
          if (title.indexOf(word) === 0) score += 60;
          else if (title.indexOf(word) > -1) score += 40;
          if (page.indexOf(word) > -1) score += 12;
          if (text.indexOf(word) > -1) score += 8;
          if (title.indexOf(word) < 0 && text.indexOf(word) < 0 && page.indexOf(word) < 0) {
            score -= 100;
          }
        }
        return { entry: entry, score: score };
      })
      .filter(function (hit) { return hit.score > 0; })
      .sort(function (a, b) { return b.score - a.score; })
      .slice(0, 24);

    if (!scored.length) {
      results.innerHTML = '<li class="empty">Nothing matched.</li>';
      return;
    }

    scored.forEach(function (hit, position) {
      var item = document.createElement("li");
      if (position === 0) item.className = "sel";
      item.innerHTML =
        '<a href="' + hit.entry.url + '">' +
        highlight(hit.entry.title, words[0]) +
        "<small>" + escapeHtml(hit.entry.page) + " · " +
        highlight(snippet(hit.entry.text || "", words[0]), words[0]) +
        "</small></a>";
      results.appendChild(item);
    });
  }

  if (modal) {
    var opener = document.querySelector(".search-open");
    if (opener) opener.addEventListener("click", openSearch);

    input.addEventListener("input", function () { render(input.value); });

    modal.addEventListener("click", function (event) {
      if (!event.target.closest(".search-panel")) closeSearch();
    });

    document.addEventListener("keydown", function (event) {
      var typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);

      if (!modal.hidden) {
        var items = results.querySelectorAll("li");
        if (event.key === "Escape") { closeSearch(); return; }
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          if (!items.length) return;
          items[selection] && items[selection].classList.remove("sel");
          selection += event.key === "ArrowDown" ? 1 : -1;
          if (selection < 0) selection = items.length - 1;
          if (selection >= items.length) selection = 0;
          var chosen = items[selection];
          if (chosen) { chosen.classList.add("sel"); chosen.scrollIntoView({ block: "nearest" }); }
          return;
        }
        if (event.key === "Enter") {
          var link = items[selection] && items[selection].querySelector("a");
          if (link) { event.preventDefault(); window.location.href = link.getAttribute("href"); }
          return;
        }
        return;
      }

      if (typing) return;
      if (event.key === "/" || ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k")) {
        event.preventDefault();
        openSearch();
      }
    });
  }

  /* ----------------------------------------------------- highlighting --- */

  var RULES = {
    sigla: [
      ["com", /#[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"/],
      ["kw", /\b(?:where|never)\b/],
      ["fn", /\b[a-z][A-Za-z0-9_]*(?:\.[a-z][A-Za-z0-9_]*)*\.[A-Z][A-Za-z0-9_]*/],
      ["num", /-?\b\d[\d_]*\b/],
      ["var", /\b[A-Z][A-Za-z0-9_]*\b/],
      ["pun", /[{}()=|!<>+\-;,?]+|\.\./]
    ],
    schema: [
      ["com", /#[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"/],
      ["kw", /\b(?:schema|predicate|import|type|derive|stored|evolves|enum|maybe|set)\b/],
      ["fn", /\b(?:int|string)\b/],
      ["num", /\b\d[\d_]*\b/],
      ["var", /\b[A-Z][A-Za-z0-9_]*\b/],
      ["pun", /->|[{}()\[\]:,|=]/]
    ],
    plan: [
      ["com", /#[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"/],
      ["kw", /\b(?:scan|seek|fetch|absent|head|where|value)\b/],
      ["var", /\br\d+#?/],
      ["num", /-?\b\d[\d_]*\b/],
      ["fn", /\b[a-z][A-Za-z0-9_]*\.[A-Z][A-Za-z0-9_]*/],
      ["pun", /<-|==|!=|>=|<=|[{}()\[\]=|+\-,.]/]
    ],
    rust: [
      ["com", /\/\/[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)'/],
      ["kw", /\b(?:as|async|await|break|const|continue|crate|dyn|else|enum|fn|for|if|impl|in|let|loop|match|mod|move|mut|pub|ref|return|self|Self|static|struct|trait|type|unsafe|use|where|while)\b/],
      ["num", /\b\d[\d_]*(?:\.\d+)?(?:[iuf](?:8|16|32|64|size))?\b/],
      ["fn", /\b[A-Z][A-Za-z0-9_]*\b/],
      ["pun", /->|=>|::|[{}()\[\]<>:;,.&*=!+\-|?]/]
    ],
    csharp: [
      ["com", /\/\/[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"/],
      ["kw", /\b(?:async|await|class|const|else|for|foreach|if|in|internal|namespace|new|null|out|override|private|public|readonly|record|return|sealed|static|struct|this|throw|using|var|void|while)\b/],
      ["num", /\b\d[\d_]*\b/],
      ["fn", /\b[A-Z][A-Za-z0-9_]*\b/],
      ["pun", /=>|[{}()\[\]<>:;,.=!+\-|?]/]
    ],
    python: [
      ["com", /#[^\n]*/],
      ["str", /"""[\s\S]*?"""|"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'/],
      ["kw", /\b(?:and|as|assert|class|def|elif|else|except|finally|for|from|if|import|in|is|lambda|none|not|or|pass|raise|return|try|while|with|yield|None|True|False)\b/],
      ["num", /\b\d[\d_]*(?:\.\d+)?\b/],
      ["fn", /\b[A-Z][A-Za-z0-9_]*\b/],
      ["pun", /[{}()\[\]:;,.=!+\-*|?]/]
    ],
    bash: [
      ["com", /#[^\n]*/],
      ["str", /"(?:[^"\\\n]|\\.)*"|'[^'\n]*'/],
      ["kw", /\b(?:cargo|python3|dotnet|fjord|fjord-viewer|git|export|cd|rm|mkdir|sleep|while|do|done|if|then|fi|kill|tar|curl|echo)\b/],
      ["num", /\b\d+\b/],
      ["fn", /(?:^|\s)--?[A-Za-z][\w-]*/],
      ["pun", /[|&;<>(){}$]/]
    ],
    json: [
      ["str", /"(?:[^"\\\n]|\\.)*"/],
      ["kw", /\b(?:true|false|null)\b/],
      ["num", /-?\b\d+(?:\.\d+)?\b/],
      ["pun", /[{}\[\]:,]/]
    ]
  };
  RULES.sh = RULES.bash;
  RULES.console = RULES.bash;
  RULES.jsonl = RULES.json;
  RULES.cs = RULES.csharp;

  function escape(text) {
    return text.replace(/[&<>]/g, function (character) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;" }[character];
    });
  }

  function compile(rules) {
    // One global regex per rule, reused with `lastIndex` — recompiling per token
    // would make a long block quadratic in rule count for no reason.
    return rules.map(function (rule) {
      return [rule[0], new RegExp(rule[1].source, "g")];
    });
  }

  function paint(source, rules) {
    var out = "";
    var at = 0;
    while (at < source.length) {
      var bestIndex = -1;
      var bestKind = null;
      var bestMatch = null;
      for (var i = 0; i < rules.length; i++) {
        var pattern = rules[i][1];
        pattern.lastIndex = at;
        var found = pattern.exec(source);
        if (found && (bestIndex === -1 || found.index < bestIndex)) {
          bestIndex = found.index;
          bestKind = rules[i][0];
          bestMatch = found[0];
        }
        if (bestIndex === at) break;
      }
      if (bestIndex === -1) { out += escape(source.slice(at)); break; }
      out += escape(source.slice(at, bestIndex));
      var lead = bestMatch.match(/^\s+/);
      if (lead) {
        out += escape(lead[0]);
        bestMatch = bestMatch.slice(lead[0].length);
      }
      out += '<span class="tok-' + bestKind + '">' + escape(bestMatch) + "</span>";
      at = bestIndex + (lead ? lead[0].length : 0) + bestMatch.length;
    }
    return out;
  }

  var COMPILED = {};
  Object.keys(RULES).forEach(function (language) {
    COMPILED[language] = compile(RULES[language]);
  });

  document.querySelectorAll("figure.code code").forEach(function (code) {
    var language = (code.className.match(/lang-([\w+-]+)/) || [])[1];
    var rules = language && COMPILED[language];
    if (!rules) return;
    code.innerHTML = paint(code.textContent, rules);
  });
})();
