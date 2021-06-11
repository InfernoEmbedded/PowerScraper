import json
import urllib.parse
from twisted.internet import defer, reactor
from twisted.web.client import Agent
from twisted.web.http_headers import Headers


##
# An HTTP error has occurred when accessing the inverter
# @param error the error
# @param context the HTTP context
def httpError(error, context):
    context.update({
        'msg': error.getErrorMessage(),
        'trace': error.getTraceback(),
        'err': error,
    })

#    print("Error: " + error.getErrorMessage + "\n Trace:\n" + error.getTraceback())
    # cancel a possible timeout
    if 'timeoutCall' in context:
        if context['timeoutCall'].active():
            context['timeoutCall'].cancel()

    return context

def nullResponse(response, context):
    # update with information we already have
    context.update({
        'status': response.code,
        'message': response.phrase,
        'headers': response.headers.getAllRawHeaders(),
    })

    # cancel a possible connect timeout
    if 'timeoutCall' in context:
        if context['timeoutCall'].active():
            context['timeoutCall'].cancel()

def requestHTTP(context={}):
    agent = Agent(reactor)
    headers = {}

    # deferred for the request
    d = agent.request(context['method'], (context['url']).encode('utf-8'), Headers(headers))
    # a timeout for the request to have run
    # NOTE: also counts time spent waiting for semaphore!
    if 'connect_timeout' in context:
        context['timeoutCall'] = \
            reactor.callLater(context['connect_timeout'], d.cancel)

    if 'callback' in context:
        d.addCallback(context['callback'], context)
    if 'errback' in context:
        d.addErrback(context['errback'], context)
    return d


class EmonCMS(object):
    def __init__(self, config):
        self.host = config['server']
        self.apikey = config['api_key']
        self.timeout = config['timeout']

    def send(self, vals, batteryAPI):
        inverterDetails = vals.copy()
        inverterDetails.pop('Serial', None)
        url = self.host + "/input/post"

        vars = {}
        vars['apikey'] = self.apikey
        vars['node'] = inverterDetails.pop('name', None)
        vars['fulljson'] = json.dumps(inverterDetails)

        url += "?" + urllib.parse.urlencode(vars)

        httpContext = {
            'method': b'GET',
            'url':  url,
            'callback': nullResponse,
            'errback':  httpError,
            'connect_timeout': self.timeout
        }

        semaphore = defer.DeferredSemaphore(1)
        semaphore.run(requestHTTP, context=httpContext)

