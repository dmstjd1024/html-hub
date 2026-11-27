//! Claude 아티팩트 중 HTML 이 아닌 유형(Markdown, SVG, Mermaid, React .jsx)을 올리면 HTML 로 감싸 저장한다.
//! 감싼 결과도 다른 문서처럼 격리(sandbox)해서 내보낸다.

use crate::auth::escape;

/// 받을 수 있는 확장자. 안내 문구와 파일 선택 칸에 같이 쓴다.
pub const ACCEPT: &str = ".html,.htm,.md,.svg,.mmd,.mermaid,.jsx";

/// 파일 이름의 확장자에 따라 HTML 로 바꾼다. 받지 않는 확장자면 None.
pub fn to_html(filename: &str, bytes: &[u8]) -> Option<Result<Vec<u8>, &'static str>> {
    let lower = filename.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e)?;
    if matches!(ext, "html" | "htm") {
        return Some(Ok(bytes.to_vec()));
    }
    let convert: fn(&str) -> String = match ext {
        "md" => markdown,
        "svg" => svg,
        "mmd" | "mermaid" => mermaid,
        "jsx" => jsx,
        _ => return None,
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Some(Err("UTF-8 로 저장된 파일만 올릴 수 있습니다."));
    };
    let name = escape(filename.rsplit(['/', '\\']).next().unwrap_or(filename));
    Some(Ok(format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{name}</title>{}",
        convert(text.trim_start_matches('\u{feff}'))
    )
    .into_bytes()))
}

const MERMAID_RUN: &str = "<script type=\"module\">import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs';\
     mermaid.initialize({startOnLoad:false,theme:matchMedia('(prefers-color-scheme: dark)').matches?'dark':'default'});await mermaid.run();</script>";

// 서버에서 그려 두면 자바스크립트 없이 보이고 본문 검색에도 걸린다.
fn markdown(text: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS));
    // ```mermaid 코드 블록은 도표로 그린다.
    let diagrams = if body.contains("class=\"language-mermaid\"") {
        format!(
            "<script>document.querySelectorAll('pre>code.language-mermaid').forEach(c=>{{const p=c.parentElement;p.className='mermaid';p.textContent=c.textContent}})</script>{MERMAID_RUN}"
        )
    } else {
        String::new()
    };
    format!(
        "<style>body{{font:16px/1.7 system-ui,-apple-system,\"Apple SD Gothic Neo\",sans-serif;max-width:760px;margin:0 auto;padding:32px 20px;color:#1f2328;word-break:keep-all;overflow-wrap:anywhere}}\
         h1,h2,h3{{line-height:1.3}}h1,h2{{border-bottom:1px solid #d1d9e0;padding-bottom:.3em}}\
         table{{border-collapse:collapse;display:block;overflow-x:auto}}th,td{{border:1px solid #d1d9e0;padding:6px 12px}}th{{background:#f6f8fa}}\
         code{{background:#eff1f3;padding:.1em .4em;border-radius:4px;font-size:.9em}}pre{{background:#f6f8fa;padding:14px;border-radius:6px;overflow-x:auto}}pre code{{background:none;padding:0}}\
         blockquote{{margin:0;padding:0 1em;color:#59636e;border-left:4px solid #d1d9e0}}img{{max-width:100%}}\
         @media (prefers-color-scheme:dark){{body{{background:#0d1117;color:#e6edf3}}th,pre{{background:#161b22}}code{{background:#262c36}}th,td,h1,h2{{border-color:#3d444d}}blockquote{{color:#9198a1;border-color:#3d444d}}}}</style>\
         </head><body>{body}{diagrams}</body></html>"
    )
}

fn svg(text: &str) -> String {
    // XML 선언과 DOCTYPE 은 HTML 안에서 쓸모가 없어 뗀다.
    let mut s = text.trim_start();
    while s.starts_with("<?") || s.starts_with("<!") {
        s = s.find('>').map_or("", |e| &s[e + 1..]).trim_start();
    }
    format!(
        "<style>body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;box-sizing:border-box}}svg{{max-width:100%;height:auto}}</style>\
         </head><body>{s}</body></html>"
    )
}

fn mermaid(text: &str) -> String {
    format!(
        "<style>body{{margin:0;padding:24px;font-family:system-ui,sans-serif}}.mermaid{{display:flex;justify-content:center}}</style>\
         </head><body><pre class=\"mermaid\">{}</pre>{MERMAID_RUN}</body></html>",
        escape(text)
    )
}

// claude.ai React 아티팩트처럼 `export default` 컴포넌트 하나를 그린다. JSX 는 브라우저에서 Babel 로 바꾸고,
// 흔히 쓰는 라이브러리는 import map 으로 esm.sh 에서 받는다. React 가 하나만 뜨도록 나머지는 react 를 external 로 둔다.
// shortcut: shadcn/ui(@/components/ui/*) 는 없다. 쓰는 아티팩트가 많아지면 흉내 낸 모듈을 넣는다.
fn jsx(text: &str) -> String {
    // 소스를 JSON 문자열로 넣는다. `<` 를 바꿔 </script> 로 일찍 닫히지 않게 한다.
    let src = serde_json::to_string(text).unwrap().replace('<', "\\u003c");
    format!(
        "<script src=\"https://cdn.tailwindcss.com\"></script>\
         <script src=\"https://unpkg.com/@babel/standalone@7/babel.min.js\"></script>\
         <script type=\"importmap\">{{\"imports\":{{\
         \"react\":\"https://esm.sh/react@18.3.1\",\
         \"react/jsx-runtime\":\"https://esm.sh/react@18.3.1/jsx-runtime\",\
         \"react-dom\":\"https://esm.sh/react-dom@18.3.1?external=react\",\
         \"react-dom/client\":\"https://esm.sh/react-dom@18.3.1/client?external=react\",\
         \"recharts\":\"https://esm.sh/recharts@2.15.0?external=react,react-dom\",\
         \"lucide-react\":\"https://esm.sh/lucide-react@0.460.0?external=react\",\
         \"lodash\":\"https://esm.sh/lodash@4.17.21\",\
         \"d3\":\"https://esm.sh/d3@7\",\
         \"mathjs\":\"https://esm.sh/mathjs@13\",\
         \"papaparse\":\"https://esm.sh/papaparse@5\",\
         \"three\":\"https://esm.sh/three@0.160.0\",\
         \"chart.js\":\"https://esm.sh/chart.js@4\",\
         \"xlsx\":\"https://esm.sh/xlsx@0.18.5\"}}}}</script>\
         </head><body><div id=\"root\"></div>\
         <script id=\"jsx-source\" type=\"application/json\">{src}</script>\
         <script type=\"module\">\
         const root=document.getElementById('root');\
         try{{\
         const src=JSON.parse(document.getElementById('jsx-source').textContent);\
         const code=Babel.transform(src,{{presets:[['react',{{runtime:'automatic'}}]],filename:'App.jsx'}}).code;\
         const mod=await import(URL.createObjectURL(new Blob([code],{{type:'text/javascript'}})));\
         const {{createElement}}=await import('react');const {{createRoot}}=await import('react-dom/client');\
         createRoot(root).render(createElement(mod.default));\
         }}catch(e){{const p=document.createElement('pre');p.style.cssText='color:#b91c1c;white-space:pre-wrap;padding:16px';\
         p.textContent='이 React 컴포넌트를 실행하지 못했습니다.\\n'+e;root.replaceChildren(p);}}\
         </script></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::to_html;

    fn html(name: &str, src: &str) -> String {
        String::from_utf8(to_html(name, src.as_bytes()).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn html_은_그대로_나머지는_감싸고_모르는_확장자는_거절한다() {
        assert_eq!(html("a.HTML", "<p>x"), "<p>x");
        assert!(to_html("a.txt", b"x").is_none());
        assert!(to_html("noext", b"x").is_none());
        assert!(matches!(to_html("a.md", &[0xff, 0xfe]), Some(Err(_))));
        for name in ["a.md", "a.svg", "a.mmd", "a.mermaid", "a.jsx"] {
            assert!(html(name, "x").starts_with("<!doctype html>"), "{name}");
        }
    }

    #[test]
    fn markdown_은_서버에서_그린다() {
        let out = html("회의록.md", "# 제목\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```mermaid\nflowchart LR\n  A-->B\n```\n");
        assert!(out.contains("<h1>제목</h1>") && out.contains("<td>1</td>"), "{out}");
        assert!(out.contains("<title>회의록.md</title>") && out.contains("mermaid.run()"));
        assert!(!html("a.md", "글").contains("mermaid"), "mermaid 블록이 없으면 스크립트도 없다");
    }

    #[test]
    fn svg_는_xml_선언을_떼고_넣는다() {
        let out = html("a.svg", "\u{feff}<?xml version=\"1.0\"?>\n<!DOCTYPE svg>\n<svg><circle/></svg>");
        assert!(out.contains("<body><svg><circle/></svg></body>") && !out.contains("<?xml"), "{out}");
    }

    #[test]
    fn mermaid_와_jsx_는_소스가_태그를_깨지_못한다() {
        assert!(html("a.mmd", "A-->B</pre><script>").contains("A--&gt;B&lt;/pre&gt;&lt;script&gt;"));
        let out = html("a.jsx", "export default () => <div>\"</script><script>alert(1)</script>\"</div>;");
        assert_eq!(out.matches("</script>").count(), 5, "감싼 스크립트 5개만 닫힌다: {out}");
        assert!(out.contains("\\u003c/script>"));
    }
}
