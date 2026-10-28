class Store {
  get(key) {
    return this.items.find(key);
  }
}

function open(name) {
  return new Store(name);
}

const close = function (store) {
  store.flush();
};
