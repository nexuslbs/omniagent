# -*- mode: ruby -*-
# vi: set ft=ruby :

require 'yaml'

config_file = File.exist?(File.join(__dir__, 'config.yml')) ? YAML.load_file(File.join(__dir__, 'config.yml')) : {}

VM_NAME   = config_file.dig('vm', 'name')   || "omniagent-vm"
VM_MEMORY = config_file.dig('vm', 'memory') || 4096
VM_CPUS   = config_file.dig('vm', 'cpus')   || 2
VM_DISK   = config_file.dig('vm', 'disk')   || "50GB"

Vagrant.configure("2") do |config|
  # ── Base Box ─────────────────────────────────────────────────────────
  config.vm.box = "generic/ubuntu2204"

  unless File.exist?(File.join(__dir__, '.vagrant/machines/default/hyperv/id'))
    # ── Primary Disk ─────────────────────────────────────────────────────
    config.vm.disk :disk, size: VM_DISK, primary: true
  end

  # ── No Host File Sharing (security) ─────────────────────────────────
  config.vm.synced_folder ".", "/vagrant", disabled: true

  # ── VM Resources ────────────────────────────────────────────────────
  config.vm.provider "virtualbox" do |vb|
    vb.memory = VM_MEMORY.to_i
    vb.maxmemory = VM_MEMORY.to_i
    vb.cpus   = VM_CPUS.to_i
    vb.name   = VM_NAME
  end

  config.vm.provider "hyperv" do |hv|
    hv.memory = VM_MEMORY.to_i
    hv.maxmemory = VM_MEMORY.to_i
    hv.cpus   = VM_CPUS.to_i
    hv.vmname = VM_NAME
    hv.enable_enhanced_session_mode = false
  end

  # ── Network ─────────────────────────────────────────────────────────
  config.vm.provider "virtualbox" do |_vb, override|
    override.vm.network "private_network", type: "dhcp"
  end

  # ── SSH ─────────────────────────────────────────────────────────────
  config.ssh.forward_agent = false
  config.ssh.insert_key = true

  # ── Disable Swap ────────────────────────────────────────────────────
  config.vm.provision "shell", name: "disable-swap", privileged: true, inline: <<-SHELL
    set -euxo pipefail
    echo "Disabling swap memory..."
    sudo swapoff -a
    sudo sed -i '/swap/d' /etc/fstab
    echo "Swap permanently disabled."
  SHELL

  # ── Install Docker Engine + Compose ─────────────────────────────────
  config.vm.provision "shell", name: "install-docker", privileged: true, inline: <<-SHELL
    set -euxo pipefail

    # Install prerequisites
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl git

    # Add Docker's official GPG key and repository
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc

    echo \\
      "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu \\
      $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \\
      tee /etc/apt/sources.list.d/docker.list > /dev/null

    # Install Docker Engine, CLI, containerd, and Compose plugin
    apt-get update -qq
    apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

    # Add vagrant user to docker group
    usermod -aG docker vagrant

    # Enable and start Docker
    systemctl enable docker
    systemctl start docker

    # Verify installation
    docker --version
    docker compose version
  SHELL

  # ── Clone Repo + Run Startup ───────────────────────────────────────
  config.vm.provision "shell", name: "setup-omniagent", privileged: true, inline: <<-SHELL
    set -euxo pipefail

    sleep 2

    # Clone this repo
    if [ ! -d /opt/omniagent ]; then
      git clone https://github.com/nexuslbs/omniagent.git /opt/omniagent
    fi

    # Run the startup script
    bash /opt/omniagent/scripts/startup.sh
  SHELL
end
