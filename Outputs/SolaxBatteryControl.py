import datetime
import os
from time import sleep, tzset

import pprint
pp = pprint.PrettyPrinter(indent=4)

class SolaxBatteryControl(object):
    def __init__(self, config):
        self.config = config
        self.phasePower = [0]*16
        self.assistNeeded = {}
        self.totalPower = 0
        self.totalDischargePower = 0
        self.maxTotalChargePower = self.maxTotalChargePower()
        self.maxTotalDischargePower = self.maxTotalDischargePower()

        if 'timezone' in config:
                os.environ['TZ'] = config['timezone']
                tzset()

    def handleMeterPower(self, vals):
        self.totalPower = vals['Total system power']
        self.phasePower[1] = vals['Phase 1 power']
        self.phasePower[2] = vals['Phase 2 power']
        self.phasePower[3] = vals['Phase 3 power']

    def handleInverterPower(self, phase, vals):
        self.phasePower[phase] = vals['Measured Power'] * -1
        self.totalPower = self.phasePower[1] + self.phasePower[2] + self.phasePower[3]

    def getPeriod(self):
        nowDateTime = datetime.datetime.now()
        now = datetime.time(nowDateTime.hour, nowDateTime.minute, nowDateTime.second)

        for periodName, period in self.config['Period'].items():
            bits = period['start'].split(':')
            start = datetime.time(int(bits[0]), int(bits[1]), int(bits[2]))

            bits = period['end'].split(':')
            end = datetime.time(int(bits[0]), int(bits[1]), int(bits[2]))


            if start < end:
                if start <= now < end:
                    return period
            else:
                if not (end <= now < start):
                    return period

        print("No period for {}\n".format(now))
        return None

    def dischargeAt(self, api, inverter, period, power):
        if 'force-discharge' in period:
            power = period['force-discharge']

        if power > inverter['max-discharge']:
            power = inverter['max-discharge']

        if power < (inverter['max-charge'] * -1):
            power = inverter['max-charge'] * -1

        if inverter['Battery Capacity'] <= period['min-charge'] and power > 0:
            power = 0
            #print("Inverter battery power clamped to 0\n")

        power = int(power) * -1
        #print("Inverter {} battery power = {}\n".format(inverter['name'], power))

        api.chargeBattery(power)

    def assistancePower(self):
        for inverter, assistNeeded in self.assistNeeded.items():
            if assistNeeded:
                return True

        return False

    def maxTotalDischargePower(self):
        power = 0

        for inverterName, inverter in self.config['Inverter'].items():
            power += inverter['max-discharge']

        return power

    def maxTotalChargePower(self):
        power = 0

        for inverterName, inverter in self.config['Inverter'].items():
            power += inverter['max-charge']

        return power

    def send(self, vals, batteryAPI):
        valCopy = vals.copy()
        inverterName = valCopy.pop('name', None)
        
        if 'source' in self.config:
            if inverterName == self.config['source']:
                self.handleMeterPower(valCopy)
                return

        if inverterName not in self.config['Inverter']:
            print("Inverter {} not found\n".format(inverterName))
            pp.pprint(self.config)
            return

        period = self.getPeriod()
        if period is None:
            return

        inverter = self.config['Inverter'][inverterName]
        if 'DischargePower' not in inverter:
            inverter['DischargePower'] = 0
            self.assistNeeded[inverterName] = False

        if 'name' not in inverter:
            inverter['name'] = inverterName

        if 'use-total-power' not in inverter:
            inverter['use-total-power'] = False

        inverter['Battery Capacity'] = vals['Battery Capacity']

        phase = inverter['phase']

        grace = False
        if 'grace' in period and 'grace-capacity' in inverter and inverter['grace-capacity'] > 0 and 'grace-charge-power' in inverter:
            grace = period['grace']

        if not 'source' in self.config:
            #print("Using inverter power")
            self.handleInverterPower(phase, valCopy)

        # If we allow charging from the grid, charge up to the minimum capacity
        if period['grid-charge'] and vals['Battery Capacity'] < period['min-charge']:
            inverter['DischargePower'] = inverter['max-charge'] * -1
            #print("{} charging from the grid at {}W".format(inverterName, inverter['DischargePower'] * -1))
            self.dischargeAt(batteryAPI, inverter, period, inverter['DischargePower'])
            # Don't expect other inverters to take the load if power is cheap enough to charge
            # otherwise, we just shift power from one inverter to another and suffer conversion losses
            # along the way
            self.assistNeeded[inverterName] = False
            return

        # If the battery is below the minimum capacity, and we prefer to charge the battery, send solar power to the battery
        if vals['Battery Capacity'] < period['min-charge'] and 'prefer-battery' in period and period['prefer-battery']:
            inverter['DischargePower'] = 0 - vals['PV1 Power'] - vals['PV2 Power']
            if (inverter['DischargePower'] < (inverter['max-charge'] * -1)):
                inverter['DischargePower'] = inverter['max-charge'] * -1

            self.dischargeAt(batteryAPI,  inverter, period, inverter['DischargePower'])
            self.assistNeeded[inverterName] = True
            return

        if inverter['use-total-power']:
            inverter['DischargePower'] += self.totalPower * 0.25
        else:
            # Try and zero our phase power
            #print("Initial discharge power is {}, additional from phase is {}\n".format(inverter['DischargePower'], self.phasePower[phase]))
            inverter['DischargePower'] += self.phasePower[phase] * 0.25

        if self.assistNeeded[inverterName]:
            if inverter['DischargePower'] >= 0 and inverter['DischargePower'] < inverter['single-phase-discharge-limit'] / len(self.config['Inverter']):
                self.assistNeeded[inverterName] = False
            elif inverter['DischargePower'] < 0 and inverter['DischargePower'] > -1 * inverter['single-phase-charge-limit'] / len(self.config['Inverter']):
                self.assistNeeded[inverterName] = False
            #else:
                #print("{} In range for 3 phase".format(inverterName))
        else:
            # Do we need help servicing the load?
            if (inverter['DischargePower'] + self.phasePower[phase] * 0.75) > inverter['single-phase-discharge-limit']:
                #print("{} Discharge exceeded {} > {}".format(inverterName, inverter['DischargePower'] + self.phasePower[phase] * 0.75, inverter['single-phase-discharge-limit']))
                self.assistNeeded[inverterName] = True
            elif (inverter['DischargePower'] + self.phasePower[phase] * 0.75) < inverter['single-phase-charge-limit'] * -1:
                #print("{} Charge exceeded {} < {}".format(inverterName, inverter['DischargePower'] + self.phasePower[phase] * 0.75, inverter['single-phase-charge-limit'] * -1))
                self.assistNeeded[inverterName] = True
            #else:
                #print("{} In range for single phase".format(inverterName))

        # Don't discharge below the minimum allowed for this period
        if vals['Battery Capacity'] <= period['min-charge'] and inverter['DischargePower'] > 0:
            inverter['DischargePower'] = 0
            #print("{} not discharging as capacity is low ({} <= {})".format(inverterName, vals['Battery Capacity'], period['min-charge']))
            self.dischargeAt(batteryAPI,  inverter, period, inverter['DischargePower'])
            self.assistNeeded[inverterName] = True
            return

        if self.assistancePower():
            # Load share between phases
            #print("Load sharing activated, Total power is {}".format(self.totalPower))
            if self.config['linked-batteries']:
                self.totalDischargePower += self.totalPower * 0.1
                inverter['DischargePower'] = self.totalDischargePower / len(self.config['Inverter'])
                if self.totalDischargePower > self.maxTotalDischargePower:
                    self.totalDischargePower = self.maxTotalDischargePower
                elif self.totalDischargePower < self.maxTotalChargePower * -1:
                    self.totalDischargePower = self.maxTotalChargePower * -1

                #print("{} totalPower delta = {}  Linked assistance power = {}".format(inverterName, self.totalPower, inverter['DischargePower']))
            else:
                inverter['DischargePower'] -= self.phasePower[phase] * 0.25
                inverter['DischargePower'] += self.totalPower * 0.1
        #else:
           #print("Phase {} power is {}".format(phase, self.phasePower[phase]))

        # Clamp charge & discharge power
        if inverter['DischargePower'] > inverter['max-discharge']:
            inverter['DischargePower'] = inverter['max-discharge']
        elif inverter['DischargePower'] < inverter['max-charge'] * -1:
            inverter['DischargePower'] = inverter['max-charge'] * -1

        # Don't consider the battery as charging if it is throttled by the BMS
        if vals['Battery Capacity'] > 95 and inverter['DischargePower'] < 0 and vals['Battery Power'] < inverter['DischargePower'] / -10:
            inverter['DischargePower'] = 0
            self.assistNeeded[inverterName] = True

        # Handle grace period
        if grace and inverter['DischargePower'] < 0 and vals['Battery Capacity'] > inverter['grace-capacity']:
            if (vals['PV1 Power'] + vals['PV2 Power']) < inverter['grace-power-threshold']:
                inverter['DischargePower'] = 0
                self.assistNeeded[inverterName] = True
            elif inverter['DischargePower'] < (inverter['grace-charge-power'] * -1):
                inverter['DischargePower'] = inverter['grace-charge-power'] * -1

        print("{} to discharge at {}W".format(inverterName, inverter['DischargePower']))
        self.dischargeAt(batteryAPI, inverter, period, inverter['DischargePower'])


