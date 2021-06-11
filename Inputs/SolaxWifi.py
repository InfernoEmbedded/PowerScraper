import json
from twisted.internet import defer, reactor
from twisted.web.client import Agent, readBody
#import unicodedata
from twisted.web.http_headers import Headers

#import pprint
#pp = pprint.PrettyPrinter(indent=4)


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


def httpResponse(response, context):
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

    # capture the body data
    body = readBody(response)

    # a timeout for the request body to be delivered
    if 'body_timeout' in context:
        context['timeoutCall'] = reactor.callLater(context['body_timeout'], body.cancel)

    body.addCallback(inverterBody, context)
    return body

def requestHTTP(context={}):
    agent = Agent(reactor)
    headers = {}

    # deferred for the request
    d = agent.request(context['method'], (context['url']).encode('utf-8'), Headers(headers))

    # a timeout for the request to have run
    # NOTE: also counts time spent waiting for semaphore!
    if 'connect_timeout' in context:
        context['timeoutCall'] = reactor.callLater(context['connect_timeout'], d.cancel)

    if 'callback' in context:
        d.addCallback(context['callback'], context)
    if 'errback' in context:
        d.addErrback(context['errback'], context)
    return d

##
# Parse the body from the inverter and chain on additional actions
# @param body the byte array body from the inverter
# @param context the HTTP request context
def inverterBody(body, context):
    str = body.decode("utf-8")
    str = str.replace(",,", ",0,")
    str = str.replace(",,", ",0,")
    metadata = json.loads(str)

    vals = {}
    vals['Serial'] = metadata['SN']
    vals['name'] = context['inverter']
    vals['PV1 Current'] = metadata['Data'][0]
    vals['PV2 Current'] = metadata['Data'][1]
    vals['PV1 Voltage'] = metadata['Data'][2]
    vals['PV2 Voltage'] = metadata['Data'][3]
    vals['Grid Current'] = metadata['Data'][4]
    vals['Grid Voltage'] = metadata['Data'][5]
    vals['Grid Power'] = metadata['Data'][6]
    vals['Inner Temp'] = metadata['Data'][7]
    vals['Solar Today'] = metadata['Data'][8]
    vals['Solar Total'] = metadata['Data'][9]
    vals['Feed In Power'] = metadata['Data'][10]
    vals['PV1 Power'] = metadata['Data'][11]
    vals['PV2 Power'] = metadata['Data'][12]
    vals['Battery Voltage'] = metadata['Data'][13]
    vals['Battery Current'] = metadata['Data'][14]
    vals['Battery Power'] = metadata['Data'][15]
    vals['Battery Temp'] = metadata['Data'][16]
    vals['Battery Capacity'] = metadata['Data'][17]
    vals['Solar Total 2'] = metadata['Data'][19]
    vals['Energy to Grid'] = metadata['Data'][41]
    vals['Energy from Grid'] = metadata['Data'][42]
    vals['Grid Frequency'] = metadata['Data'][50]
    vals['EPS Voltage'] = metadata['Data'][53]
    vals['EPS Current'] = metadata['Data'][54]
    vals['EPS VA'] = metadata['Data'][55]
    vals['EPS Frequency'] = metadata['Data'][56]
    vals['Status'] = metadata['Data'][67]

    context['completionCallback'](vals, None)


class SolaxWifi(object):

    ##
    # Create a new class to fetch data from the Wifi interface of Solax inverters
    def __init__(self, host, timeout):
        self.host = host
        self.timeout = timeout

    def fetch(self, completionCallback):
        semaphore = defer.DeferredSemaphore(1)

        httpContext = {
            'inverter': self.host,
            'method': b'GET',
            'url':      'http://' + self.host + '/api/realTimeData.htm',
            'callback': httpResponse,
            'errback':  httpError,
            'completionCallback': completionCallback,
            'connect_timeout':  self.timeout,
        }

        request = semaphore.run(requestHTTP, context=httpContext)
        return request
