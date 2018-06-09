#!/usr/bin/python3

# Scrapes Inverter information from Solax inverters and presents it to OpenEnergyMonitor
#
# Setup:
#   pip install toml twisted
#   cp config-example.toml config.toml
#   vi config.toml 
#
# Copyright ©2018 Inferno Embedded   http://infernoembedded.com
# 
# This program is free software: you can redistribute it and/or modify
#    it under the terms of the GNU General Public License as published by
#    the Free Software Foundation, either version 3 of the License, or
#    (at your option) any later version.
# 
#    This program is distributed in the hope that it will be useful,
#    but WITHOUT ANY WARRANTY; without even the implied warranty of
#    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
#    GNU General Public License for more details.
# 
#    You should have received a copy of the GNU General Public License
#    along with this program.  If not, see <https://www.gnu.org/licenses/>.

import toml
from twisted.internet import task, defer, reactor
from twisted.internet.protocol import Protocol
from twisted.web.client import Agent, readBody
from twisted.web.http_headers import Headers
import unicodedata
import json
import urllib.parse

import pprint

# from twisted.internet.defer import setDebugging
# setDebugging(True)

pp = pprint.PrettyPrinter(indent=4)

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

    print("Error: " + error.getErrorMessage + "\n Trace:\n" + error.getTraceback())

    # cancel a possible timeout
    if 'timeoutCall' in context:
        if context['timeoutCall'].active():
            context['timeoutCall'].cancel()

    return context


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
    vals['inverter'] = context['inverter']
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
    
#    pp.pprint(vals)
    print("Got data for inverter '" + vals['inverter'] + "'");
    
    sendToEmonCMS(vals)


##
# Register inverter information with EmonCMS
# @param inverterDetails a dictionary of inverter information
def sendToEmonCMS(inverterDetails):
    if not 'emoncms' in config:
        return
    
    inverterDetails = inverterDetails.copy()
 
    url = config['emoncms']['server'] + "/input/post"
    
    inverterDetails.pop('Serial', None)
    
    vars = {}
    vars['apikey'] = config['emoncms']['api_key']
    vars['node'] = inverterDetails.pop('inverter', None)
    vars['fulljson'] = json.dumps(inverterDetails)

    url += "?" + urllib.parse.urlencode(vars)

    httpContext = {
        'method': b'GET',
        'url':  url,
        'callback': nullResponse,
        'errback':  httpError,
        'connect_timeout':  config['timeout']
    }

    semaphore = defer.DeferredSemaphore(1)
    semaphore.run(requestHTTP, context=httpContext)
    

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
        context['timeoutCall'] = \
            reactor.callLater(context['connect_timeout'], d.cancel)

    if 'callback' in context:
        d.addCallback(context['callback'], context)
    if 'errback' in context:
        d.addErrback(context['errback'], context)
    return d
           
def actions():
    requests = []
    semaphore = defer.DeferredSemaphore(len(config['inverters']))
    
    for inverter in config['inverters']:
        print("Checking inverter " + inverter + "\n")
        httpContext = {
            'inverter': inverter,
            'method': b'GET',
            'url':      'http://' + inverter + '/api/realTimeData.htm',
            'callback': httpResponse,
            'errback':  httpError,
            'connect_timeout':  config['timeout']
        }
        request = semaphore.run(requestHTTP, context=httpContext)
        requests.append(request)


with open("config.toml") as conffile:
    global config
    config = toml.loads(conffile.read())
    
#    pp.pprint(config)

looper = task.LoopingCall(actions)
looper.start(config['poll_period'])

reactor.run()
