import os


class App:
    def __init__(self):
        self.routes = {}

    def get(self, path):
        def wrap(fn):
            self.routes[("GET", path)] = fn
            return fn
        return wrap


class Conn:
    def subscribe(self, subject, cb):
        self.cb = cb

    def publish(self, subject, data):
        print(subject, data)


class Counter:
    def bump(self):
        raise NotImplementedError


class MemCounter(Counter):
    def __init__(self):
        self.n = 0

    def bump(self):
        self.n += 1
        return self.n


app = App()
nc = Conn()
counter = MemCounter()
LIMIT = int(os.environ.get("POLY_LIMIT", "10"))


def on_created(msg):
    n = counter.bump()
    if n > LIMIT:
        nc.publish("stats.updated", str(n))


@app.get("/api/stats")
def stats():
    return {"count": counter.n}


nc.subscribe("entities.created", cb=on_created)


def test_on_created():
    on_created("x")
    assert counter.n == 1


def test_purge_is_announced():
    nc.publish("entities.purged", "x")
