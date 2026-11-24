import { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection } from "@codemirror/view";
import { EditorState, type Extension, type Text } from "@codemirror/state";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { bracketMatching, indentOnInput, StreamLanguage, syntaxHighlighting, defaultHighlightStyle } from "@codemirror/language";
import { oneDark } from "@codemirror/theme-one-dark";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { json } from "@codemirror/lang-json";
import { html } from "@codemirror/lang-html";
import { css } from "@codemirror/lang-css";
import { markdown } from "@codemirror/lang-markdown";
import { yaml } from "@codemirror/lang-yaml";
import { xml } from "@codemirror/lang-xml";
import { sql } from "@codemirror/lang-sql";
import { shell } from "@codemirror/legacy-modes/mode/shell";
import { nginx } from "@codemirror/legacy-modes/mode/nginx";
import { properties } from "@codemirror/legacy-modes/mode/properties";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { dockerFile } from "@codemirror/legacy-modes/mode/dockerfile";
import { go } from "@codemirror/legacy-modes/mode/go";
import { rust } from "@codemirror/legacy-modes/mode/rust";
import { ruby } from "@codemirror/legacy-modes/mode/ruby";
import { c, cpp, java } from "@codemirror/legacy-modes/mode/clike";

/** 파일 이름/경로로 문법 강조 종류를 고른다. */
function languageFor(path: string): Extension {
  const name = path.split("/").pop()!.toLowerCase();
  const ext = name.includes(".") ? name.split(".").pop()! : "";
  const legacy = (m: Parameters<typeof StreamLanguage.define>[0]) => StreamLanguage.define(m);

  if (name === "dockerfile") return legacy(dockerFile);
  if (/^\.(bashrc|bash_profile|bash_logout|profile|zshrc|zprofile|env)$/.test(name) || ["sh", "bash", "zsh", "env"].includes(ext)) return legacy(shell);
  switch (ext) {
    case "js": case "mjs": case "cjs": case "jsx": return javascript({ jsx: true });
    case "ts": case "tsx": return javascript({ jsx: true, typescript: true });
    case "py": return python();
    case "json": return json();
    case "html": case "htm": return html();
    case "css": case "scss": return css();
    case "md": case "markdown": return markdown();
    case "yml": case "yaml": return yaml();
    case "xml": case "svg": return xml();
    case "sql": return sql();
    case "toml": return legacy(toml);
    case "go": return legacy(go);
    case "rs": return legacy(rust);
    case "rb": return legacy(ruby);
    case "c": case "h": return legacy(c);
    case "cpp": case "cc": case "hpp": return legacy(cpp);
    case "java": return legacy(java);
    case "conf": return path.includes("nginx") ? legacy(nginx) : legacy(properties);
    case "ini": case "cfg": case "properties": case "service": return legacy(properties);
  }
  return [];
}

export interface Ed {
  view: EditorView;
  /** 현재 내용을 저장본으로 표시 */
  markSaved(): void;
  isDirty(): boolean;
  value(): string;
  /** 내용을 통째로 바꾸고 저장본으로 표시 (서버 버전 다시 불러오기) */
  replace(text: string): void;
}

export function createEditor(
  parent: HTMLElement, doc: string, path: string,
  cb: { onDirty: (dirty: boolean) => void; onSave: () => void },
): Ed {
  let saved: Text;
  const ed: Ed = {
    view: null as unknown as EditorView,
    markSaved() { saved = ed.view.state.doc; cb.onDirty(false); },
    isDirty: () => !ed.view.state.doc.eq(saved),
    value: () => ed.view.state.doc.toString(),
    replace(text) {
      ed.view.dispatch({ changes: { from: 0, to: ed.view.state.doc.length, insert: text } });
      ed.markSaved();
    },
  };

  ed.view = new EditorView({
    parent,
    state: EditorState.create({
      doc,
      extensions: [
        lineNumbers(), highlightActiveLineGutter(), highlightActiveLine(), drawSelection(),
        history(), indentOnInput(), bracketMatching(), highlightSelectionMatches(),
        syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
        oneDark,
        languageFor(path),
        keymap.of([
          { key: "Mod-s", preventDefault: true, run: () => { cb.onSave(); return true; } },
          indentWithTab, ...defaultKeymap, ...historyKeymap, ...searchKeymap,
        ]),
        EditorView.updateListener.of((u) => { if (u.docChanged) cb.onDirty(ed.isDirty()); }),
        EditorView.theme({
          "&": { height: "100%", backgroundColor: "#14161b" },
          ".cm-scroller": { fontFamily: 'Menlo, "SF Mono", Consolas, monospace', fontSize: "13px", lineHeight: "1.55" },
          ".cm-gutters": { backgroundColor: "#14161b", border: "none" },
        }),
      ],
    }),
  });
  saved = ed.view.state.doc;
  return ed;
}
