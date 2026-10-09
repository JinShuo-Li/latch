import {escapeHtml as e} from './state.js';

// Find an exact matching backtick run; unmatched ticks remain ordinary text.
function codeEnd(text,start,size) {
  for(let i=start;i<text.length;) {
    if(text[i]!=='`'){i++;continue;}
    let end=i+1;while(text[end]==='`')end++;
    if(end-i===size)return i;
    i=end;
  }
  return -1;
}
function inline(text) {
  let html='';
  for(let i=0;i<text.length;) {
    if(text[i]==='\\' && /[\\|`*]/.test(text[i+1] || '')) {html+=e(text[i+1]);i+=2;continue;}
    if(text[i]==='`') {
      let end=i+1;while(text[end]==='`')end++;
      const size=end-i;
      const close=codeEnd(text,end,size);
      if(close!==-1){html+=`<code>${e(text.slice(end,close).replaceAll('\\|','|'))}</code>`;i=close+size;continue;}
      html+=e(text.slice(i,end));i=end;continue;
    }
    if(text.startsWith('**',i)) {
      const end=text.indexOf('**',i+2);
      if(end>i+2){html+=`<strong>${inline(text.slice(i+2,end))}</strong>`;i=end+2;continue;}
    }
    html+=e(text[i]);i++;
  }
  return html;
}
function tableRow(line) {
  const text=line.trim();
  const cells=[];let cell='';let pipes=0;
  for(let i=0;i<text.length;) {
    if(text[i]==='\\' && i+1<text.length){cell+=text.slice(i,i+2);i+=2;continue;}
    if(text[i]==='`') {
      let end=i+1;while(text[end]==='`')end++;
      const size=end-i;
      const close=codeEnd(text,end,size);
      if(close!==-1){const after=close+size;cell+=text.slice(i,after);i=after;continue;}
      cell+=text.slice(i,end);i=end;continue;
    }
    if(text[i]==='|'){cells.push(cell.trim());cell='';pipes++;i++;continue;}
    cell+=text[i++];
  }
  if(!pipes)return null;
  cells.push(cell.trim());
  if(text.startsWith('|'))cells.shift();
  // An escaped final pipe stays in the cell, so only remove an empty edge cell.
  if(text.endsWith('|') && cells.at(-1)==='')cells.pop();
  return cells.length?cells:null;
}
function table(lines,start) {
  const header=tableRow(lines[start]);
  const separator=tableRow(lines[start+1] || '');
  if(!header || !separator || header.length!==separator.length || !separator.every(cell=>/^:?-{3,}:?$/.test(cell)))return null;
  const align=separator.map(cell=>cell.startsWith(':') && cell.endsWith(':')?'center':cell.endsWith(':')?'right':'left');
  const row=(cells,tag)=>`<tr>${header.map((_,i)=>`<${tag}${tag==='th'?' scope="col"':''} class="align-${align[i]}">${inline(cells[i] || '')}</${tag}>`).join('')}</tr>`;
  let html=`<div class="markdown-table-scroll" role="region" aria-label="Table" tabindex="0"><table class="markdown-table"><thead>${row(header,'th')}</thead><tbody>`;
  let end=start+2;
  while(end<lines.length) {
    const cells=tableRow(lines[end]);
    if(!cells || /^\s*(?:```|~~~|#{1,6}\s|[-*]\s)/.test(lines[end]))break;
    html+=row(cells,'td');end++;
  }
  return {html:html+'</tbody></table></div>',end};
}
export function markdown(text) {
  const lines=String(text || '').replaceAll('\r\n','\n').split('\n');
  let code=false;let list=false;let html='';
  for(let i=0;i<lines.length;i++) {
    const line=lines[i];
    if(line.startsWith('```')){if(list){html+='</ul>';list=false;}html+=code?'</code></pre>':'<pre class="code-block"><code>';code=!code;continue;}
    if(code){html+=e(line)+'\n';continue;}
    const block=table(lines,i);
    if(block){if(list){html+='</ul>';list=false;}html+=block.html;i=block.end-1;continue;}
    if(/^[-*] /.test(line)){if(!list){html+='<ul>';list=true;}html+=`<li>${inline(line.slice(2))}</li>`;continue;}
    if(list){html+='</ul>';list=false;}
    if(/^#{1,4} /.test(line))html+=`<h3>${inline(line.replace(/^#+ /,''))}</h3>`;
    else if(line.trim())html+=`<p>${inline(line)}</p>`;
  }
  if(code)html+='</code></pre>';
  if(list)html+='</ul>';
  return html;
}
