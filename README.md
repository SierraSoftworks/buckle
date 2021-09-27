# Buckle
**Minimum viable bootstrapping for your infrastructure, with great observability**

Buckle is an extremely lightweight infrastructure bootstrapping agent designed to operate
as an alternative to more complex (and powerful) tools like Ansible, Puppet, Chef and SaltStack.

Where Buckle shines is in situations where you need to configure a host quickly, but really
don't want to maintain all of the "fluff" that comes with a full blown configuration management
system. A great example of this is using the [CustomScriptExtension](https://docs.microsoft.com/en-us/azure/virtual-machines/extensions/custom-script-linux)
on Azure to bootstrap or upgrade a machine.

Buckle is written in Rust and takes advantage of the great OpenTelemetry integration provided
by [tracing.rs](https://tracing.rs/tracing/) to provide centralized visibility into your deployments.

## Features
 - Lightweight agent written in Rust.
 - Exceptional execution tracing through OpenTelemetry.
 - Dynamic configuration loading.
 - Packages with dependency-driven execution ordering.
 - File templating.
 - No special knowledge required to operate (it's just running bash scripts).

## Configuration
Buckle is responsible for applying one or more configuration packages to your system. These packages
are discovered from your provided `--config DIR` using a filesystem layout which looks like the following.

```
.
└── my-config/
    │
    ├── config/
    │   ├── defaults.env
    │   └── azure-vmss.sh
    │
    ├── secrets/
    │   └── logging-keys.env
    │
    └── packages/
        ├── pkg1/
        │   ├── config/
        │   │   └── versions.env
        │   ├── files/
        │   │   ├── confd/
        │   │   │   └── myapp.conf
        │   │   └── systemd/
        │   │       └── myapp.service
        │   ├── scripts/
        │   │   └── enable-service.sh
        │   └── package.yml
        │
        ├── pkg2/
        │   ├── files/
        │   │   └── confd/
        │   │       └── logging.conf
        │   └── package.yml
        │
        └── pkg3/
            ├── scripts/
            │   └── install.sh
            └── package.yml
```

There are several key directories which you fill find here:

### `config`
Files within this directory will be read by Buckle and their contents exposed as environment
variables within your scripts and templates. The file extension used will determine how the file is read,
with the following file extensions currently supported:

- `.env` files are read line-by-line as a sequence of `KEY=value` pairs.
- `.sh` files are executed with the system's `bash` interpreter and their stdout parsed line-by-line as a sequence of `KEY=value` pairs.
- `.ps1` files are executed with the system's `pwsh` interpreter and their stdout parsed line-by-line as a sequence of `KEY=value` pairs.
- `.bat` files are executed with the system's `cmd.exe` interpreter and their stdout parsed line-by-line as a sequence of `KEY=value` pairs.
- `.cmd` files are executed with the system's `cmd.exe` interpreter and their stdout parsed line-by-line as a sequence of `KEY=value` pairs.

This means that it is possible to write scripts which will retrieve information about the current
environment, including calling local metadata services etc.

You can define config at the global level, as well as the package level. All packages will inherit the global
config fields you provide and will overlay their own config on top of those.

#### Examples

##### `defaults.env`
```
DEBIAN_FRONTEND=noninteractive
```

##### `azure-vmss.sh`
This config script will attempt to retrieve the location, IP address and subnet that the Azure VMSS instance is running on.

```bash
LOCATION="$(curl -m 5 -H Metadata:true --noproxy "*" "http://169.254.169.254/metadata/instance?api-version=2020-09-01" 2>/dev/null | jq -r '.compute.location')"
IP_ADDRESS="$(curl -m 5 -H Metadata:true --noproxy "*" "http://169.254.169.254/metadata/instance?api-version=2020-09-01" 2>/dev/null | jq -r '.network.interface[0].ipv4.ipAddress[0].privateIpAddress')"
SUBNET="$(curl -m 5 -H Metadata:true --noproxy "*" "http://169.254.169.254/metadata/instance?api-version=2020-09-01" 2>/dev/null | jq -r '.network.interface[0].ipv4.subnet[0].address + "/" + .network.interface[0].ipv4.subnet[0].prefix')"

echo "LOCATION=$LOCATION"
echo "IP_ADDRESS=$IP_ADDRESS"
echo "SUBNET=$SUBNET"
```

### `secrets`
Secrets behave identically to the `config` directory, however their contents are not emitted by Buckle to your logging/telemetry.
This helps avoid inadvertently exposing those secrets, however **Buckle does not strip secrets from your script output**. The values
provided by secrets take precedence over their config counterparts (when a secret and config have the same name, the secret's value will
win).

### `packages`
Packages are the unit of bootstrapping that Buckle relies on. Packages are intended to be composable and
self-contained so that they can easily be re-used between projects. At a high-level, a package is composed
of some metadata describing the package and its dependencies, the configuration and secrets which apply
to the package, the files the package will generate, and the scripts which will be executed to set up the package.

```
.
├── config/
│   └── versions.env
├── files/
│   ├── confd/
│   │   └── myapp.conf
│   └── systemd/
│       └── myapp.service
├── scripts/
│   └── enable-service.sh
└── package.yml
```

#### `packages.yml`
The metadata file describing the package should be a YAML file containing the following structure:

```yaml
description: |
    This is a description of what the package installs and anything that
    an operator should be aware of when using the package.

# You can list other packages which should be installed before this one
needs:
    - pkg1
    - pkg2

# Here you provide the mapping between files in your package and the
# target host's filesystem.
files:
    # Files within the ./files/confd/ directory should be placed in /etc/myservice.d/
    confd: /etc/myservice.d
    systemd: /etc/systemd/system
```

#### `files/`
The files directory should contain a series of subdirectories which correspond to the
`package.yml#files` map's keys. In the example above, we should expect to find two directories
with the names `confd` and `systemd`. Files within these directories will be placed on
the host filesystem in the directories listed in `package.yml`. *Rich directory structures
are also supported and will be accurately reflected on the host filesystem.*

##### Templates
At times, it can be useful to generate the content of these files dynamically. Buckle supports
this use case for files that have the `.tpl` file extension. These files will have the `.tpl`
extension stripped and their contents templated using Go's [template/text](https://pkg.go.dev/text/template)
templating language. Any of your configuration variables will be accessible like this: `{{ .IP_ADDRESS }}`.

#### `scripts/`
The scripts directory should contain any scripts you wish to execute on the host system
when applying this package. Scripts should use one of the supported file extensions below:

- `.sh` files are executed with the system's `bash` interpreter.
- `.ps1` files are executed with the system's `pwsh` interpreters.
- `.bat` files are executed with the system's `cmd.exe` interpreter.
- `.cmd` files are executed with the system's `cmd.exe` interpreter.

Scripts are executed *after* files have been placed on the host.