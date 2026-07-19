// The app bundle: reads EAGER-DATA (like naver's first screen) and
// renders the feed with React, with a stateful click interaction.
(function () {
  var e = React.createElement;
  var data = window['EAGER-DATA'] && window['EAGER-DATA']['NEWS'];
  var items = (data && data.items) || [];

  function Feed() {
    var s = React.useState(0);
    var picked = s[0], setPicked = s[1];
    return e('div', {id: 'feed'},
      e('h2', null, '뉴스 헤드라인'),
      e('ul', null, items.map(function (it, i) {
        return e('li', {key: String(i)},
          e('a', {
            href: it.url,
            className: 'headline',
            onClick: function (ev) {
              ev.preventDefault();
              setPicked(i + 1);
            }
          }, it.title));
      })),
      e('p', {id: 'picked'}, '선택: ' + String(picked)));
  }

  var host = document.getElementById('root');
  if (ReactDOM.createRoot) {
    ReactDOM.createRoot(host).render(e(Feed));
  } else {
    ReactDOM.render(e(Feed), host);
  }
  console.log('FIXTURE-APP-MOUNT-QUEUED');
})();
