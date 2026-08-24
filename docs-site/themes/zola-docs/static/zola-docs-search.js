function rankSearchDocs(docs, query) {
  var terms = query.toLowerCase().trim().split(/\s+/).filter(Boolean);
  if (!terms.length) return [];
  return docs.map(function (doc) {
    var title = (doc.title || "").toLowerCase();
    var body = (doc.body || "").toLowerCase();
    var path = (doc.path || "").toLowerCase();
    if (!terms.every(function (term) {
      return title.includes(term) || body.includes(term) || path.includes(term);
    })) return null;
    var score = terms.reduce(function (total, term) {
      return total
        + (title.includes(term) ? 30 : 0)
        + (path.includes(term) ? 8 : 0)
        + Math.min(body.split(term).length - 1, 8);
    }, 0);
    return { doc: doc, score: score };
  }).filter(Boolean).sort(function (left, right) {
    return right.score - left.score
      || (left.doc.title || "").localeCompare(right.doc.title || "");
  });
}

function searchSnippet(body, terms, radius) {
  var normalized = body || "";
  var lower = normalized.toLowerCase();
  var positions = terms.map(function (term) { return lower.indexOf(term.toLowerCase()); }).filter(function (position) { return position >= 0; });
  if (!positions.length) return normalized.slice(0, radius * 2).trim();
  var start = Math.max(0, Math.min.apply(Math, positions) - radius);
  var end = Math.min(normalized.length, start + radius * 2);
  return (start ? "…" : "") + normalized.slice(start, end).trim() + (end < normalized.length ? "…" : "");
}

function appendHighlighted(parent, text, terms) {
  var pattern = new RegExp("(" + terms.map(function (term) {
    return term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  }).join("|") + ")", "ig");
  text.split(pattern).forEach(function (part) {
    if (terms.some(function (term) { return part.toLowerCase() === term.toLowerCase(); })) {
      var mark = document.createElement("mark");
      mark.textContent = part;
      parent.appendChild(mark);
    } else {
      parent.appendChild(document.createTextNode(part));
    }
  });
}

(function () {
  if (typeof document === "undefined") return;
  var form = document.querySelector("[data-search-form]");
  if (!form) return;
  var input = form.querySelector("[data-search-input]");
  var panel = form.querySelector("[data-search-panel]");
  var status = form.querySelector("[data-search-status]");
  var list = form.querySelector("[data-search-results]");
  var indexPromise;
  var activeIndex = -1;

  function setOpen(open) {
    panel.hidden = !open;
    input.setAttribute("aria-expanded", String(open));
  }

  function loadIndex() {
    if (!indexPromise) {
      indexPromise = fetch(form.dataset.searchIndex).then(function (response) {
        if (!response.ok) throw new Error("Search index request failed");
        return response.json();
      });
    }
    return indexPromise;
  }

  async function search() {
    var query = input.value.trim();
    if (query.length < input.minLength) {
      setOpen(false);
      return;
    }
    status.textContent = "Searching";
    setOpen(true);
    try {
      var limit = Number(form.dataset.searchLimit) || 10;
      var matches = rankSearchDocs(await loadIndex(), query).slice(0, limit);
      var terms = query.toLowerCase().split(/\s+/).filter(Boolean);
      list.replaceChildren();
      activeIndex = -1;
      status.textContent = matches.length ? matches.length + " results shown" : "No results";
      matches.forEach(function (match) {
        var item = document.createElement("li");
        var link = document.createElement("a");
        link.href = match.doc.url;
        link.textContent = match.doc.title || match.doc.path;
        item.appendChild(link);
        if (match.doc.path) {
          var path = document.createElement("span");
          path.className = "search-result-path";
          path.textContent = match.doc.path;
          item.appendChild(path);
        }
        if (match.doc.body) {
          var snippet = document.createElement("p");
          snippet.className = "search-result-snippet";
          appendHighlighted(snippet, searchSnippet(match.doc.body, terms, 90), terms);
          item.appendChild(snippet);
        }
        list.appendChild(item);
      });
    } catch (_error) {
      list.replaceChildren();
      status.textContent = "Search is unavailable";
    }
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    search();
  });

  input.addEventListener("input", function () {
    search();
  });
  document.addEventListener("keydown", function (event) {
    var target = event.target;
    if (event.key === "/" && !target.closest("input, textarea, select, [contenteditable=true]")) {
      event.preventDefault();
      input.focus();
      return;
    }
    if (event.key === "Escape" && !panel.hidden) {
      setOpen(false);
      input.focus();
      return;
    }
    if (!panel.hidden && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
      var links = Array.from(list.querySelectorAll("a"));
      if (!links.length) return;
      event.preventDefault();
      activeIndex = event.key === "ArrowDown"
        ? (activeIndex + 1) % links.length
        : (activeIndex - 1 + links.length) % links.length;
      links[activeIndex].focus();
    }
  });
  document.addEventListener("click", function (event) {
    if (!form.contains(event.target)) setOpen(false);
  });
})();

if (typeof module !== "undefined") module.exports = { rankSearchDocs: rankSearchDocs, searchSnippet: searchSnippet };
