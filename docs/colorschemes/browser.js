(function () {
  var root = document.querySelector("[data-scheme-browser]");
  if (!root) {
    return;
  }

  var searchEl = root.querySelector("[data-scheme-search]");
  var sourceEl = root.querySelector("[data-scheme-source]");
  var appearanceEl = root.querySelector("[data-scheme-appearance]");
  var matchEl = root.querySelector("[data-scheme-match]");
  var pickerEl = root.querySelector("[data-scheme-picker]");
  var summaryEl = root.querySelector("[data-scheme-summary]");
  var listEl = root.querySelector("[data-scheme-list]");
  var detailEl = root.querySelector("[data-scheme-detail]");

  var url = new URL(window.location.href);
  var state = {
    catalog: [],
    filtered: [],
    prefix: (url.searchParams.get("prefix") || "").toLowerCase(),
    selected: url.searchParams.get("scheme") || (url.hash ? url.hash.slice(1) : null),
  };

  function escapeHtml(text) {
    return String(text).replace(/[&<>"']/g, function (char) {
      return {
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      }[char];
    });
  }

  function normalize(text) {
    return String(text || "").toLowerCase();
  }

  // -- CIELAB color math --
  var labCache = {};

  function hexToRgb(hex) {
    hex = hex.replace(/^#/, "");
    if (hex.length === 3) {
      hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
    }
    var n = parseInt(hex, 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }

  function linearize(c) {
    c /= 255;
    return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  }

  function hexToLab(hex) {
    if (labCache[hex]) return labCache[hex];
    var rgb = hexToRgb(hex);
    var r = linearize(rgb[0]), g = linearize(rgb[1]), b = linearize(rgb[2]);
    var x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.95047;
    var y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
    var z = (0.0193339 * r + 0.1191920 * g + 0.9503041 * b) / 1.08883;
    function f(t) { return t > 0.008856 ? Math.pow(t, 1 / 3) : 7.787 * t + 16 / 116; }
    var lab = [116 * f(y) - 16, 500 * (f(x) - f(y)), 200 * (f(y) - f(z))];
    labCache[hex] = lab;
    return lab;
  }

  function deltaE(a, b) {
    var dL = a[0] - b[0], da = a[1] - b[1], db = a[2] - b[2];
    return Math.sqrt(dL * dL + da * da + db * db);
  }

  function parseMatchColors(text) {
    return text.split(/[,;\s]+/).map(function (t) {
      t = t.trim().toLowerCase();
      if (t && t[0] !== "#") t = "#" + t;
      return t;
    }).filter(function (t) {
      return /^#[0-9a-f]{6}$/.test(t) || /^#[0-9a-f]{3}$/.test(t);
    }).map(function (t) {
      if (t.length === 4) return "#" + t[1] + t[1] + t[2] + t[2] + t[3] + t[3];
      return t;
    });
  }

  function scoreScheme(item, targetLabs) {
    var palette = [item.bg, item.fg, item.cursor, item.selection_bg, item.selection_fg]
      .concat(item.ansi).concat(item.brights);
    var paletteLabs = palette.map(hexToLab);
    var total = 0;
    for (var i = 0; i < targetLabs.length; i++) {
      var minDist = Infinity;
      for (var j = 0; j < paletteLabs.length; j++) {
        var d = deltaE(targetLabs[i], paletteLabs[j]);
        if (d < minDist) minDist = d;
      }
      total += minDist;
    }
    return total / targetLabs.length;
  }

  function currentQuery() {
    return normalize(searchEl.value.trim());
  }

  function ensureSelected() {
    var selected = state.filtered.find(function (item) {
      return item.ident === state.selected;
    });
    if (!selected) {
      state.selected = state.filtered.length ? state.filtered[0].ident : null;
    }
  }

  function makeStyleVars(item) {
    var vars = [
      "--scheme-bg:" + item.bg,
      "--scheme-fg:" + item.fg,
      "--scheme-cursor:" + item.cursor,
      "--scheme-selection-fg:" + item.selection_fg,
      "--scheme-selection-bg:" + item.selection_bg,
    ];
    item.ansi.concat(item.brights).forEach(function (color, idx) {
      vars.push("--scheme-ansi-" + idx + ":" + color);
    });
    return vars.join(";");
  }

  function renderSwatches(colors, className, labels) {
    return colors
      .map(function (color, idx) {
        var label = labels ? '<span>' + idx + "</span>" : "";
        return (
          '<span class="' +
          className +
          '" style="background:' +
          color +
          '">' +
          label +
          "</span>"
        );
      })
      .join("");
  }

  function renderSummary() {
    var pieces = [state.filtered.length + " matching schemes"];
    if (state.prefix) {
      pieces.push('prefix "' + state.prefix.toUpperCase() + '"');
    }
    if (sourceEl.value) {
      pieces.push(sourceEl.value);
    }
    if (appearanceEl.value) {
      pieces.push(appearanceEl.value);
    }
    if (parseMatchColors(matchEl.value).length > 0) {
      pieces.push("ranked by similarity");
    }
    summaryEl.textContent = pieces.join(" · ");
  }

  function syncUrl() {
    var next = new URL(window.location.href);
    if (state.prefix) {
      next.searchParams.set("prefix", state.prefix);
    } else {
      next.searchParams.delete("prefix");
    }
    if (searchEl.value.trim()) {
      next.searchParams.set("q", searchEl.value.trim());
    } else {
      next.searchParams.delete("q");
    }
    if (sourceEl.value) {
      next.searchParams.set("source", sourceEl.value);
    } else {
      next.searchParams.delete("source");
    }
    if (appearanceEl.value) {
      next.searchParams.set("appearance", appearanceEl.value);
    } else {
      next.searchParams.delete("appearance");
    }
    var matchHexes = parseMatchColors(matchEl.value);
    if (matchHexes.length) {
      next.searchParams.set("match", matchHexes.map(function (c) {
        return c.replace("#", "");
      }).join(","));
    } else {
      next.searchParams.delete("match");
    }
    next.searchParams.delete("scheme");
    next.hash = state.selected ? state.selected : "";
    history.replaceState(null, "", next);
  }

  function renderList() {
    if (!state.filtered.length) {
      listEl.innerHTML = '<p class="scheme-browser__empty">No schemes match those filters.</p>';
      return;
    }

    listEl.innerHTML = state.filtered
      .map(function (item) {
        var active = item.ident === state.selected ? " is-active" : "";
        var mini = [item.bg, item.fg].concat(item.ansi.slice(1, 5));
        var scoreBadge = "";
        if (item._matchScore !== undefined) {
          scoreBadge =
            '<span class="scheme-browser__match-score">\u0394E ' +
            item._matchScore.toFixed(1) +
            "</span>";
        }
        return (
          '<button class="scheme-browser__item' +
          active +
          '" data-scheme-select="' +
          item.ident +
          '">' +
          '<span class="scheme-browser__item-header">' +
          '<span class="scheme-browser__item-name">' +
          escapeHtml(item.name) +
          "</span>" +
          scoreBadge +
          "</span>" +
          '<span class="scheme-browser__item-meta">' +
          escapeHtml(item.source) +
          " · " +
          escapeHtml(item.appearance) +
          "</span>" +
          '<span class="scheme-browser__mini-swatches">' +
          renderSwatches(mini, "scheme-browser__mini-chip", false) +
          "</span>" +
          "</button>"
        );
      })
      .join("");
  }

  function renderDetail() {
    if (!state.filtered.length || !state.selected) {
      detailEl.innerHTML =
        '<p class="scheme-browser__empty">No schemes match those filters.</p>';
      return;
    }

    var item = state.filtered.find(function (entry) {
      return entry.ident === state.selected;
    });
    if (!item) {
      return;
    }

    var badges = [
      '<span class="scheme-browser__badge">' + escapeHtml(item.source) + "</span>",
      '<span class="scheme-browser__badge">' + escapeHtml(item.appearance) + "</span>",
    ];
    if (item.wakterm_version && item.wakterm_version !== "Always") {
      badges.push(
        '<span class="scheme-browser__badge">Since ' +
          escapeHtml(item.wakterm_version) +
          "</span>"
      );
    }

    var aliases = "";
    if (item.aliases && item.aliases.length) {
      aliases =
        '<p class="scheme-browser__aliases">Also known as ' +
        item.aliases.map(escapeHtml).join(", ") +
        "</p>";
    }

    var meta = [];
    if (item.author) {
      meta.push("Author: " + escapeHtml(item.author));
    }
    if (item.origin_url) {
      meta.push(
        'Source: <a href="' +
          encodeURI(item.origin_url) +
          '">' +
          escapeHtml(item.origin_url) +
          "</a>"
      );
    }

    detailEl.innerHTML =
      '<div class="scheme-browser__header">' +
      '<div class="scheme-browser__title">' +
      "<h2>" +
      escapeHtml(item.name) +
      "</h2>" +
      '<div class="scheme-browser__badges">' +
      badges.join("") +
      "</div>" +
      "</div>" +
      aliases +
      (meta.length
        ? '<p class="scheme-browser__meta">' + meta.join(" · ") + "</p>"
        : "") +
      "</div>" +
      '<div class="scheme-preview" style="' +
      makeStyleVars(item) +
      '">' +
      '<div class="scheme-preview__frame">' +
      '<div class="scheme-preview__chrome">' +
      '<span class="scheme-preview__chrome-dot"></span>' +
      '<span class="scheme-preview__chrome-dot"></span>' +
      '<span class="scheme-preview__chrome-dot"></span>' +
      '<span class="scheme-preview__chrome-title">' +
      escapeHtml(item.name) +
      "</span>" +
      "</div>" +
      '<div class="scheme-preview__screen">' +
      '<div><span class="scheme-preview__fg-2">mihai@wakterm</span> <span class="scheme-preview__fg-4">~/demo</span> <span class="scheme-preview__fg-3">$</span> wakterm cli agent watch</div>' +
      '<div class="scheme-preview__muted">layout restored · 4 panes attached · 1 mux server</div>' +
      '<div><span class="scheme-preview__fg-6">smoke-codex</span> <span class="scheme-preview__fg-5">final-answer</span> codex smoke ok</div>' +
      '<div><span class="scheme-preview__selection">selected text</span> <span class="scheme-preview__cursor">&nbsp;</span></div>' +
      "</div>" +
      "</div>" +
      "</div>" +
      '<div class="scheme-browser__swatches">' +
      renderSwatches(item.ansi.concat(item.brights), "scheme-browser__chip", true) +
      "</div>" +
      '<div class="scheme-browser__config"><pre><code>' +
      escapeHtml("config.color_scheme = " + JSON.stringify(item.name)) +
      "</code></pre></div>";
  }

  function applyFilters() {
    var query = currentQuery();
    var source = sourceEl.value;
    var appearance = appearanceEl.value;

    state.filtered = state.catalog.filter(function (item) {
      if (state.prefix && item.prefix !== state.prefix) {
        return false;
      }
      if (source && item.source !== source) {
        return false;
      }
      if (appearance && item.appearance !== appearance) {
        return false;
      }
      if (!query) {
        return true;
      }
      var haystack = [
        item.name,
        item.source,
        item.author || "",
        (item.aliases || []).join(" "),
      ]
        .join(" ")
        .toLowerCase();
      return haystack.indexOf(query) !== -1;
    });

    var matchHexes = parseMatchColors(matchEl.value);
    var matchLabs = matchHexes.map(hexToLab);

    if (matchLabs.length > 0) {
      state.filtered.forEach(function (item) {
        item._matchScore = scoreScheme(item, matchLabs);
      });
      state.filtered.sort(function (a, b) {
        return a._matchScore - b._matchScore;
      });
    } else {
      state.filtered.forEach(function (item) {
        item._matchScore = undefined;
      });
    }

    ensureSelected();
    renderSummary();
    renderList();
    renderDetail();
    syncUrl();
  }

  function populateSources() {
    var seen = {};
    state.catalog.forEach(function (item) {
      seen[item.source] = true;
    });
    Object.keys(seen)
      .sort(function (a, b) {
        return a.localeCompare(b);
      })
      .forEach(function (source) {
        var option = document.createElement("option");
        option.value = source;
        option.textContent = source;
        sourceEl.appendChild(option);
      });
  }

  listEl.addEventListener("click", function (event) {
    var target = event.target.closest("[data-scheme-select]");
    if (!target) {
      return;
    }
    state.selected = target.getAttribute("data-scheme-select");
    renderList();
    renderDetail();
    syncUrl();
  });

  [searchEl, sourceEl, appearanceEl, matchEl].forEach(function (el) {
    el.addEventListener("input", applyFilters);
    el.addEventListener("change", applyFilters);
  });

  pickerEl.addEventListener("input", function () {
    var color = pickerEl.value;
    var existing = matchEl.value.trim();
    matchEl.value = existing ? existing + ", " + color : color;
    applyFilters();
  });

  window.addEventListener("hashchange", function () {
    state.selected = window.location.hash ? window.location.hash.slice(1) : null;
    ensureSelected();
    renderList();
    renderDetail();
  });

  fetch(new URL("./catalog.json", window.location.href))
    .then(function (response) {
      return response.json();
    })
    .then(function (catalog) {
      state.catalog = catalog;
      populateSources();

      var urlState = new URL(window.location.href);
      searchEl.value = urlState.searchParams.get("q") || "";
      sourceEl.value = urlState.searchParams.get("source") || "";
      appearanceEl.value = urlState.searchParams.get("appearance") || "";
      matchEl.value = (urlState.searchParams.get("match") || "")
        .split(",").filter(Boolean).map(function (c) { return "#" + c; }).join(", ");

      applyFilters();
    })
    .catch(function (error) {
      detailEl.innerHTML =
        '<p class="scheme-browser__empty">Unable to load color schemes: ' +
        escapeHtml(error.message || error) +
        "</p>";
      listEl.innerHTML = "";
      summaryEl.textContent = "";
    });
})();
