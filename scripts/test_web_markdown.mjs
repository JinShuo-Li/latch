import assert from 'node:assert/strict';
import {test} from 'node:test';
import {markdown} from '../web/app/markdown.js';

test('tables render headers, alignment, formatting and surrounding blocks',()=>{
  const html=markdown('Before\n\n| Name | Value | Notes |\n| :--- | ---: | :---: |\n| **One** | `42` | 中文 |\n\nAfter');
  assert.match(html,/<thead><tr><th scope="col" class="align-left">Name<\/th><th scope="col" class="align-right">Value<\/th><th scope="col" class="align-center">Notes<\/th>/);
  assert.match(html,/<strong>One<\/strong>/);assert.match(html,/<code>42<\/code>/);
  assert.match(html,/<p>Before<\/p>.*<table.*<\/table>.*<p>After<\/p>/);
});
test('optional edge pipes, escaped pipes and pipes inside code do not shift columns',()=>{
  const html=markdown('Name | Value\n--- | ---\nA\\|B | `x|y`\n``a|b`` | end\\|\nmissing |\nextra | value | discarded');
  assert.match(html,/<td class="align-left">A\|B<\/td><td class="align-left"><code>x\|y<\/code><\/td>/);
  assert.match(html,/<code>a\|b<\/code>/);assert.match(html,/>end\|<\/td>/);
  assert.match(html,/>missing<\/td><td class="align-left"><\/td>/);
  assert.doesNotMatch(html,/discarded/);
  assert.equal((html.match(/<td /g)||[]).length,8);
});
test('code blocks, ordinary pipes and invalid separators stay ordinary text',()=>{
  assert.doesNotMatch(markdown('```md\n| A | B |\n| --- | --- |\n```'),/<table/);
  assert.doesNotMatch(markdown('a | b\n--- | invalid'),/<table/);
  assert.doesNotMatch(markdown('a | b\n--- | --- | ---'),/<table/);
  assert.doesNotMatch(markdown('ordinary | sentence'),/<table/);
});
test('streamed incomplete tables transition safely to a table',()=>{
  assert.doesNotMatch(markdown('| A | B |\n| --'),/<table/);
  assert.match(markdown('| A | B |\n| --- | --- |'),/<thead>/);
  assert.match(markdown('| A | B |\n| --- | --- |\n| next |'),/>next<\/td><td class="align-left"><\/td>/);
  assert.match(markdown('- item\n| A | B |\n| --- | --- |\n| row | value |'),/<\/ul><div class="markdown-table-scroll"/);
});
test('table cells and inline code cannot inject markup',()=>{
  const html=markdown('| <img src=x onerror=alert(1)> | **safe** |\n| --- | --- |\n| `<script>bad</script>` | & " \' |');
  assert.doesNotMatch(html,/<img|<script|onerror="/);
  assert.match(html,/&lt;img/);assert.match(html,/<code>&lt;script&gt;bad&lt;\/script&gt;<\/code>/);
  assert.match(html,/&amp; &quot; &#39;/);
  assert.equal(markdown('`**literal**`'),'<p><code>**literal**</code></p>');
});
